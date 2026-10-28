//! Health inputs shared by the API and schedule admission (BS-028.T3).
//!
//! The model itself is pure and lives in `bergsmith-maintenance`; this module
//! only presents a ledger row as its input and reduces an evaluation to the
//! findings a policy trigger checks.

use std::collections::HashMap;

use bergsmith_common::health::{HealthFinding, HealthThresholds, HealthVerdict};
use bergsmith_common::ids::{OrganizationId, PolicyId, TableId};
use bergsmith_maintenance::{HealthState, TableHealth, TableSample, ThresholdSource, evaluate};
use bergsmith_persistence::statistics::StoredTableSample;
use bergsmith_persistence::{PersistenceError, schedules, statistics};
use chrono::{DateTime, Utc};
use sqlx::PgConnection;

/// The stored sample as the health model's input.
///
/// The ledger stores counters as `bigint` and the histogram and partitions as
/// JSON; a row that cannot be presented is a ledger defect.
///
/// # Errors
///
/// [`PersistenceError::CorruptValue`] when a counter is negative or the stored
/// JSON does not describe its type.
pub fn sample_model(stored: &StoredTableSample) -> Result<TableSample, PersistenceError> {
    Ok(TableSample {
        metadata_location: stored.metadata_location.clone(),
        snapshot_id: stored.snapshot_id,
        last_operation: stored.last_operation.clone(),
        record_count: counter("record count", stored.record_count)?,
        data_files: counter("data files", stored.data_files)?,
        data_bytes: counter("data bytes", stored.data_bytes)?,
        delete_files: counter("delete files", stored.delete_files)?,
        delete_bytes: counter("delete bytes", stored.delete_bytes)?,
        position_delete_files: counter("position delete files", stored.position_delete_files)?,
        equality_delete_files: counter("equality delete files", stored.equality_delete_files)?,
        position_delete_records: counter(
            "position delete records",
            stored.position_delete_records,
        )?,
        equality_delete_records: counter(
            "equality delete records",
            stored.equality_delete_records,
        )?,
        manifest_count: counter("manifest count", stored.manifest_count)?,
        data_manifest_count: counter("data manifest count", stored.data_manifest_count)?,
        manifest_bytes: counter("manifest bytes", stored.manifest_bytes)?,
        partition_count: counter("partition count", stored.partition_count)?,
        max_partition_files: counter("max partition files", stored.max_partition_files)?,
        small_files: counter("small files", stored.small_files)?,
        target_file_size_bytes: counter("target file size", stored.target_file_size_bytes)?,
        size_histogram: serde_json::from_value(stored.size_histogram.clone()).map_err(|error| {
            PersistenceError::CorruptValue {
                entity: "table statistics histogram",
                value: error.to_string(),
            }
        })?,
        top_partitions: serde_json::from_value(stored.top_partitions.clone()).map_err(|error| {
            PersistenceError::CorruptValue {
                entity: "table statistics partitions",
                value: error.to_string(),
            }
        })?,
        snapshot_count: counter("snapshot count", stored.snapshot_count)?,
        oldest_snapshot_at: stored.oldest_snapshot_at,
        newest_snapshot_at: stored.newest_snapshot_at,
    })
}

/// The verdict a policy trigger is checked against.
#[must_use]
pub fn verdict(health: &TableHealth) -> HealthVerdict {
    HealthVerdict::new(
        health
            .findings
            .iter()
            .map(|finding| HealthFinding {
                kind: finding.kind,
                severity: finding.severity,
            })
            .collect(),
    )
}

fn counter(entity: &'static str, value: i64) -> Result<u64, PersistenceError> {
    u64::try_from(value).map_err(|_| PersistenceError::CorruptValue {
        entity,
        value: value.to_string(),
    })
}

/// Which threshold set produced an assessment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThresholdProvenance {
    /// The documented defaults in `HealthThresholds`.
    Defaults,
    /// One policy's overrides, merged over the defaults.
    Policy {
        /// The policy that supplied the set.
        id: PolicyId,
        /// Its display name, so a finding can name its source.
        name: String,
    },
}

/// One table's health as a page reads it.
///
/// A table without a complete sample has no evaluation and no source: missing
/// evidence is reported as unassessed, never as healthy (ADR 0059).
#[derive(Clone, Debug, PartialEq)]
pub struct Assessment {
    /// The worst evaluation across the table's enabled policies, if assessed.
    pub health: Option<TableHealth>,
    /// Where that evaluation's thresholds came from, if assessed.
    pub source: Option<ThresholdProvenance>,
    /// When the newest sample was taken, if there is one.
    pub sampled_at: Option<DateTime<Utc>>,
    /// When the metadata was last confirmed fresh, if there is a sample.
    pub confirmed_at: Option<DateTime<Utc>>,
}

impl Assessment {
    /// The worst state, or `None` when the table is not assessed.
    #[must_use]
    pub fn state(&self) -> Option<HealthState> {
        self.health.as_ref().map(|health| health.state)
    }

    fn unassessed(sampled_at: Option<DateTime<Utc>>, confirmed_at: Option<DateTime<Utc>>) -> Self {
        Self {
            health: None,
            source: None,
            sampled_at,
            confirmed_at,
        }
    }
}

/// Assesses each of `table_ids` from the ledger at the caller's clock.
///
/// Three batched reads (newest samples, confirmations, and the enabled
/// policies' threshold sets) serve a page or a whole inventory. A table's
/// state is the worst evaluation across its enabled policies' threshold sets,
/// and the documented defaults apply when it has none. Findings record the
/// policy that supplied their thresholds, so a page is never calmer than a
/// trigger would act on.
///
/// A sample or threshold set that cannot be presented is logged and treated as
/// unassessed: one defect in the ledger must not fail a page that lists many
/// tables.
///
/// `now` is the request's clock. One request evaluates every page, chunk, and
/// table at the same instant, so a response cannot classify otherwise equal
/// tables differently across a staleness boundary.
///
/// # Errors
///
/// [`PersistenceError`] when a ledger read fails.
pub async fn assess(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_ids: &[TableId],
    now: DateTime<Utc>,
) -> Result<HashMap<TableId, Assessment>, PersistenceError> {
    let mut assessments: HashMap<TableId, Assessment> = table_ids
        .iter()
        .map(|table| (*table, Assessment::unassessed(None, None)))
        .collect();
    if table_ids.is_empty() {
        return Ok(assessments);
    }
    let samples = statistics::latest_for_tables(conn, organization_id, table_ids).await?;
    let checks = statistics::checks_for_tables(conn, organization_id, table_ids).await?;
    let thresholds: Vec<schedules::TableThresholds> =
        schedules::enabled_thresholds_for_tables(conn, organization_id, table_ids).await?;
    let checks: HashMap<TableId, DateTime<Utc>> = checks.into_iter().collect();
    let mut sets: HashMap<TableId, Vec<schedules::TableThresholds>> = HashMap::new();
    for set in thresholds {
        sets.entry(set.table_id).or_default().push(set);
    }
    for (table, stored) in samples {
        let assessment = assess_table(
            table,
            &stored,
            checks.get(&table).copied(),
            sets.remove(&table).unwrap_or_default(),
            now,
        );
        assessments.insert(table, assessment);
    }
    Ok(assessments)
}

/// One table's assessment from its newest sample and threshold sets.
///
/// A sample written before the health counters existed carries zeros the
/// model cannot tell from measurements, so it is not assessed; the collector
/// replaces it with a complete observation on its next pass. The worst
/// evaluation across the sets wins, and the defaults apply when the table has
/// no enabled policy.
pub(crate) fn assess_table(
    table: TableId,
    stored: &StoredTableSample,
    checked: Option<DateTime<Utc>>,
    sets: Vec<schedules::TableThresholds>,
    now: DateTime<Utc>,
) -> Assessment {
    let sampled_at = Some(stored.sampled_at);
    let confirmed = checked.map_or(stored.sampled_at, |checked| checked.max(stored.sampled_at));
    if stored.sample_version < statistics::SAMPLE_VERSION {
        return Assessment::unassessed(sampled_at, Some(confirmed));
    }
    let sample = match sample_model(stored) {
        Ok(sample) => sample,
        Err(error) => {
            tracing::debug!(
                table_id = %table,
                error = %error,
                "health sample cannot be presented; reporting the table unassessed"
            );
            return Assessment::unassessed(sampled_at, Some(confirmed));
        }
    };
    let mut chosen: Option<(TableHealth, ThresholdProvenance)> = None;
    for set in sets {
        let thresholds = match set.thresholds() {
            Ok(thresholds) => thresholds,
            Err(error) => {
                tracing::debug!(
                    table_id = %table,
                    policy_id = %set.policy_id,
                    error = %error,
                    "health thresholds cannot be decoded; skipping that set"
                );
                continue;
            }
        };
        let evaluation = evaluate(
            &sample,
            confirmed,
            &thresholds,
            ThresholdSource::Policy(set.policy_id),
            now,
        );
        match evaluation {
            Ok(health) => {
                let worse = chosen
                    .as_ref()
                    .is_none_or(|(current, _)| health.state > current.state);
                if worse {
                    chosen = Some((
                        health,
                        ThresholdProvenance::Policy {
                            id: set.policy_id,
                            name: set.policy_name,
                        },
                    ));
                }
            }
            Err(error) => tracing::debug!(
                table_id = %table,
                policy_id = %set.policy_id,
                error = %error,
                "health thresholds cannot be evaluated; skipping that set"
            ),
        }
    }
    let chosen = chosen.or_else(|| {
        evaluate(
            &sample,
            confirmed,
            &HealthThresholds::default(),
            ThresholdSource::Defaults,
            now,
        )
        .map(|health| (health, ThresholdProvenance::Defaults))
        .map_err(|error| {
            tracing::debug!(
                table_id = %table,
                error = %error,
                "health defaults cannot be evaluated; reporting the table unassessed"
            );
        })
        .ok()
    });
    match chosen {
        Some((health, source)) => Assessment {
            health: Some(health),
            source: Some(source),
            sampled_at,
            confirmed_at: Some(confirmed),
        },
        None => Assessment::unassessed(sampled_at, Some(confirmed)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bergsmith_common::health::Thresholds;
    use bergsmith_common::ids::{CatalogConnectionId, PolicyId};
    use chrono::TimeZone;
    use serde_json::json;
    use uuid::Uuid;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn table() -> TableId {
        TableId::from_uuid(Uuid::now_v7())
    }

    /// Healthy under the defaults: 200 MiB average files, four snapshots.
    fn stored(sampled_at: DateTime<Utc>) -> StoredTableSample {
        StoredTableSample {
            sampled_at,
            sample_version: statistics::SAMPLE_VERSION,
            catalog_connection_id: CatalogConnectionId::from_uuid(Uuid::now_v7()),
            metadata_location: "s3://bucket/table/metadata/v1.json".to_owned(),
            snapshot_id: Some(1),
            last_operation: Some("append".to_owned()),
            record_count: 5,
            data_files: 5,
            data_bytes: 5 * 200 * 1024 * 1024,
            delete_files: 0,
            delete_bytes: 0,
            position_delete_files: 0,
            equality_delete_files: 0,
            position_delete_records: 0,
            equality_delete_records: 0,
            manifest_count: 2,
            data_manifest_count: 2,
            manifest_bytes: 2_048,
            partition_count: 0,
            max_partition_files: 0,
            small_files: 0,
            target_file_size_bytes: 512 * 1024 * 1024,
            snapshot_count: 4,
            oldest_snapshot_at: Some(sampled_at),
            newest_snapshot_at: Some(sampled_at),
            size_histogram: json!([]),
            top_partitions: json!([]),
        }
    }

    fn set(
        policy: PolicyId,
        name: &str,
        thresholds: &HealthThresholds,
    ) -> schedules::TableThresholds {
        schedules::TableThresholds::new(table(), policy, name.to_owned(), thresholds)
    }

    #[test]
    fn repeated_unusable_samples_and_thresholds_log_only_debug_diagnostics() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tracing_subscriber::prelude::*;
        #[derive(Clone)]
        struct Events {
            warnings: Arc<AtomicUsize>,
            debug: Arc<AtomicUsize>,
        }
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Events {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                match *event.metadata().level() {
                    tracing::Level::WARN => {
                        self.warnings.fetch_add(1, Ordering::Relaxed);
                    }
                    tracing::Level::DEBUG => {
                        self.debug.fetch_add(1, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        }
        let events = Events {
            warnings: Arc::default(),
            debug: Arc::default(),
        };
        let subscriber = tracing_subscriber::registry().with(events.clone());
        tracing::subscriber::with_default(subscriber, || {
            let mut corrupt = stored(at());
            corrupt.size_histogram = json!({"not": "an array"});
            let invalid = HealthThresholds {
                small_files: Thresholds {
                    warning: 0,
                    critical: 0,
                },
                ..HealthThresholds::default()
            };
            for _ in 0..3 {
                assess_table(table(), &corrupt, Some(at()), vec![], at());
                assess_table(
                    table(),
                    &stored(at()),
                    Some(at()),
                    vec![set(PolicyId::generate(), "bad", &invalid)],
                    at(),
                );
            }
        });
        assert_eq!(events.warnings.load(Ordering::Relaxed), 0);
        assert_eq!(events.debug.load(Ordering::Relaxed), 6);
    }

    #[test]
    fn the_worst_enabled_set_decides_and_names_its_policy() {
        let table = table();
        let strict_id = PolicyId::from_uuid(Uuid::now_v7());
        let lenient_id = PolicyId::from_uuid(Uuid::now_v7());
        let strict = HealthThresholds {
            small_files: Thresholds {
                warning: 2 * 1024 * 1024 * 1024,
                critical: 1024 * 1024 * 1024,
            },
            ..HealthThresholds::default()
        };
        let lenient = HealthThresholds {
            small_files: Thresholds {
                warning: 1024 * 1024,
                critical: 512 * 1024,
            },
            ..HealthThresholds::default()
        };
        let assessment = assess_table(
            table,
            &stored(at()),
            Some(at()),
            vec![
                set(strict_id, "strict", &strict),
                set(lenient_id, "lenient", &lenient),
            ],
            at(),
        );
        assert_eq!(assessment.state(), Some(HealthState::Critical));
        assert_eq!(
            assessment.source,
            Some(ThresholdProvenance::Policy {
                id: strict_id,
                name: "strict".to_owned()
            })
        );
    }

    #[test]
    fn equally_severe_sets_keep_the_first_in_policy_order() {
        let table = table();
        let first_id = PolicyId::from_uuid(Uuid::now_v7());
        let second_id = PolicyId::from_uuid(Uuid::now_v7());
        let warning = HealthThresholds {
            small_files: Thresholds {
                warning: 2 * 1024 * 1024 * 1024,
                critical: 1024 * 1024 * 1024,
            },
            ..HealthThresholds::default()
        };
        let assessment = assess_table(
            table,
            &stored(at()),
            Some(at()),
            vec![
                set(first_id, "first", &warning),
                set(second_id, "second", &warning),
            ],
            at(),
        );
        assert_eq!(
            assessment.source,
            Some(ThresholdProvenance::Policy {
                id: first_id,
                name: "first".to_owned()
            })
        );
    }

    #[test]
    fn staleness_is_judged_at_the_callers_clock() {
        let table = table();
        let sample = stored(at());
        let confirmed = at();
        let fresh = assess_table(
            table,
            &sample,
            Some(confirmed),
            Vec::new(),
            at() + chrono::TimeDelta::minutes(59),
        );
        assert_eq!(fresh.state(), Some(HealthState::Healthy));
        let warning = assess_table(
            table,
            &sample,
            Some(confirmed),
            Vec::new(),
            at() + chrono::TimeDelta::minutes(60),
        );
        assert_eq!(warning.state(), Some(HealthState::Warning));
        let critical = assess_table(
            table,
            &sample,
            Some(confirmed),
            Vec::new(),
            at() + chrono::TimeDelta::hours(6),
        );
        assert_eq!(critical.state(), Some(HealthState::Critical));
    }

    #[test]
    fn an_unusable_sample_is_unassessed_with_its_instants() {
        let table = table();
        let mut incomplete = stored(at());
        incomplete.sample_version = 0;
        let assessment = assess_table(table, &incomplete, Some(at()), Vec::new(), at());
        assert_eq!(assessment.state(), None);
        assert_eq!(assessment.source, None);
        assert_eq!(assessment.sampled_at, Some(at()));
        assert_eq!(assessment.confirmed_at, Some(at()));

        let mut corrupt = stored(at());
        corrupt.size_histogram = json!({"not": "an array"});
        assert_eq!(
            assess_table(table, &corrupt, Some(at()), Vec::new(), at()).state(),
            None
        );
    }
}
