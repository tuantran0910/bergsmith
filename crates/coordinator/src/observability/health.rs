//! Bounded health/statistics exposition from an independent ledger snapshot.

use std::collections::{BTreeMap, HashMap};

use bergsmith_common::ids::{OrganizationId, TableId};
use bergsmith_maintenance::HealthState;
use bergsmith_persistence::{PersistenceError, metrics, schedules, statistics};
use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use super::exposition::{Exposition, Kind};
use super::{MetricsConfig, value};
use crate::health;

const STATES: [&str; 4] = ["healthy", "warning", "critical", "unassessed"];
const COUNTERS: [&str; 7] = [
    "data_files",
    "data_bytes",
    "delete_files",
    "delete_bytes",
    "records",
    "manifests",
    "snapshots",
];

#[derive(Clone, Debug, Default)]
struct Group {
    states: [u64; 4],
    counters: [f64; 7],
}

#[derive(Clone, Debug)]
struct Table {
    id: Uuid,
    state: usize,
    counters: Option<[f64; 7]>,
    confirmed_at: Option<DateTime<Utc>>,
}

impl Table {
    fn new(
        id: Uuid,
        assessed: Option<(statistics::StoredTableSample, health::Assessment)>,
    ) -> Self {
        let state = assessed
            .as_ref()
            .and_then(|(_, assessment)| assessment.state())
            .map_or(3, |state| match state {
                HealthState::Healthy => 0,
                HealthState::Warning => 1,
                HealthState::Critical => 2,
            });
        // Unknown/legacy counters must not be presented as measurements.
        let counters = assessed.as_ref().filter(|_| state != 3).map(|(s, _)| {
            [
                value(s.data_files),
                value(s.data_bytes),
                value(s.delete_files),
                value(s.delete_bytes),
                value(s.record_count),
                value(s.manifest_count),
                value(s.snapshot_count),
            ]
        });
        Self {
            id,
            state,
            counters,
            confirmed_at: assessed.and_then(|(_, a)| a.confirmed_at),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct Snapshot {
    all: Group,
    connections: BTreeMap<Uuid, Group>,
    tables: BTreeMap<Uuid, Table>,
    omitted: u64,
}

impl Snapshot {
    pub(super) async fn read(
        conn: &mut PgConnection,
        config: MetricsConfig,
        now: DateTime<Utc>,
    ) -> Result<Self, PersistenceError> {
        let mut result = Self::default();
        // Empty connections still emit zero series. Only identifiers become labels.
        for id in metrics::connection_ids(conn).await? {
            result.connections.insert(id, Group::default());
        }
        let mut after = None;
        loop {
            let page = metrics::inventory_page(conn, after).await?;
            if page.is_empty() {
                break;
            }
            after = page.last().map(|table| (table.organization_id, table.id));
            // Group each bounded page by tenant; the existing repositories enforce scope.
            let mut tenants = BTreeMap::<Uuid, Vec<_>>::new();
            for table in page {
                tenants
                    .entry(table.organization_id)
                    .or_default()
                    .push(table);
            }
            for (organization, inventory) in tenants {
                let organization = OrganizationId::from_uuid(organization);
                let ids: Vec<_> = inventory
                    .iter()
                    .map(|table| TableId::from_uuid(table.id))
                    .collect();
                let mut samples: HashMap<_, _> =
                    statistics::latest_for_tables(conn, organization, &ids)
                        .await?
                        .into_iter()
                        .collect();
                let checks: HashMap<_, _> = statistics::checks_for_tables(conn, organization, &ids)
                    .await?
                    .into_iter()
                    .collect();
                let mut sets = HashMap::<TableId, Vec<_>>::new();
                for set in
                    schedules::enabled_thresholds_for_tables(conn, organization, &ids).await?
                {
                    sets.entry(set.table_id).or_default().push(set);
                }
                for item in inventory {
                    let id = TableId::from_uuid(item.id);
                    let assessed = samples.remove(&id).map(|sample| {
                        let assessment = health::assess_table(
                            id,
                            &sample,
                            checks.get(&id).copied(),
                            sets.remove(&id).unwrap_or_default(),
                            now,
                        );
                        (sample, assessment)
                    });
                    let table = Table::new(item.id, assessed);
                    let (state, counters) = (table.state, table.counters);
                    result.all.add(state, counters);
                    for connection in item.connections {
                        result
                            .connections
                            .entry(connection)
                            .or_default()
                            .add(state, counters);
                    }
                    result.keep(table, config);
                }
            }
        }
        Ok(result)
    }

    fn keep(&mut self, table: Table, config: MetricsConfig) {
        if !config.per_table_metrics {
            return;
        }
        if self.tables.len() < config.max_table_metrics {
            self.tables.insert(table.id, table);
        } else {
            self.omitted += 1;
            if self
                .tables
                .last_key_value()
                .is_some_and(|(id, _)| table.id < *id)
            {
                let _ = self.tables.pop_last();
                self.tables.insert(table.id, table);
            }
        }
    }

    pub(super) fn write(&self, out: &mut Exposition) {
        out.family("bergsmith_tables_health", Kind::Gauge, "Canonical tables by current health state; missing or incomplete samples are unassessed.");
        write_states(out, "bergsmith_tables_health", &[], &self.all);
        out.family(
            "bergsmith_connection_tables_health",
            Kind::Gauge,
            "Distinct tables named by each connection, by current health state.",
        );
        for (id, group) in &self.connections {
            write_states(
                out,
                "bergsmith_connection_tables_health",
                &[("connection_id", &id.to_string())],
                group,
            );
        }
        for (index, counter) in COUNTERS.iter().enumerate() {
            let name = format!("bergsmith_statistics_{counter}");
            out.family(
                &name,
                Kind::Gauge,
                "Sum of newest complete samples over canonical tables; unassessed tables excluded.",
            );
            out.sample(&name, &[], self.all.counters[index]);
            let name = format!("bergsmith_connection_statistics_{counter}");
            out.family(&name, Kind::Gauge, "Sum of newest complete samples over distinct tables in a connection; shared tables count in each connection.");
            for (id, group) in &self.connections {
                out.sample(
                    &name,
                    &[("connection_id", &id.to_string())],
                    group.counters[index],
                );
            }
        }
        if !self.tables.is_empty() || self.omitted > 0 {
            out.family(
                "bergsmith_table_health",
                Kind::Gauge,
                "Opt-in table health: healthy=0, warning=1, critical=2, unassessed=3.",
            );
            for table in self.tables.values() {
                let id = table.id.to_string();
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "state ranks are bounded to 0 through 3"
                )]
                out.sample(
                    "bergsmith_table_health",
                    &[("table_id", &id)],
                    table.state as f64,
                );
            }
            out.family("bergsmith_table_statistics_confirmed_timestamp_seconds", Kind::Gauge, "Last metadata confirmation of the newest sample, as Unix seconds; absent without a sample.");
            for table in self.tables.values() {
                let id = table.id.to_string();
                if let Some(at) = table.confirmed_at {
                    out.sample(
                        "bergsmith_table_statistics_confirmed_timestamp_seconds",
                        &[("table_id", &id)],
                        value(at.timestamp()),
                    );
                }
            }
            for (index, counter) in COUNTERS.iter().enumerate() {
                let name = format!("bergsmith_table_statistics_{counter}");
                out.family(
                    &name,
                    Kind::Gauge,
                    "Opt-in newest complete table sample; absent when unassessed.",
                );
                for table in self.tables.values() {
                    if let Some(counters) = table.counters {
                        out.sample(
                            &name,
                            &[("table_id", &table.id.to_string())],
                            counters[index],
                        );
                    }
                }
            }
        }
        out.family("bergsmith_table_metrics_omitted", Kind::Gauge, "Tables omitted by the individual-series cap; zero when individual series are disabled.");
        #[expect(
            clippy::cast_precision_loss,
            reason = "Prometheus represents counts as floats"
        )]
        out.sample("bergsmith_table_metrics_omitted", &[], self.omitted as f64);
    }
}

impl Group {
    fn add(&mut self, state: usize, counters: Option<[f64; 7]>) {
        self.states[state] += 1;
        if let Some(counters) = counters {
            for (sum, count) in self.counters.iter_mut().zip(counters) {
                *sum += count;
            }
        }
    }
}

fn write_states(out: &mut Exposition, name: &str, labels: &[(&str, &str)], group: &Group) {
    for (index, state) in STATES.iter().enumerate() {
        let mut labels = labels.to_vec();
        labels.push(("state", state));
        #[expect(
            clippy::cast_precision_loss,
            reason = "Prometheus represents counts as floats"
        )]
        out.sample(name, &labels, group.states[index] as f64);
    }
}
