//! Metadata-only statistics sampling (BS-025.T1).
//!
//! [`StatisticsCollector`] samples every registered table whose latest
//! confirmation is older than the interval, plus the table behind each
//! publication. A sample is read from table metadata and manifests only; the
//! collector never reads a data file. Every successful read records a
//! confirmation, so an unchanged table stays fresh without a new sample.
//!
//! One pass walks due tables grouped by catalog connection and samples each
//! connection's tables in turn, so a slow catalog sees one request at a time
//! and connections make progress independently. A failure scoped to one table
//! is logged and leaves that table's previous sample authoritative.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bergsmith_catalog::TableName;
use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, TableId};
use bergsmith_maintenance::{StatisticsConfig, TableSample, snapshot_history};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::jobs::Job;
use bergsmith_persistence::statistics::{
    self, DueTable, MaintenanceRoute, NewTableSample, NewTableSnapshot,
};
use bergsmith_persistence::tables;
use chrono::{DateTime, TimeDelta, Utc};
use futures::StreamExt;
use sqlx::{Connection, PgPool};
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, warn};

use super::catalogs::{AccessError, RuntimeCatalogs, TableBinding};

/// How many expired samples one pass deletes, so pruning stays short.
const PRUNE_BATCH: i64 = 1_000;

/// How many of a table's aliases one sample tries, so a stale or unreachable
/// catalog does not stop the table's statistics while another alias works.
const MAX_ROUTE_ATTEMPTS: usize = 3;

/// How the collector spaces and bounds its samples.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SamplingSettings {
    /// How old the newest sample must be before the table is sampled again.
    pub interval_millis: u64,
    /// How long a sample is kept.
    pub retention_days: u64,
    /// How many due tables one pass examines.
    pub page_size: i64,
    /// How many catalog connections one pass samples concurrently.
    pub max_concurrent_connections: usize,
    /// The bounds of one sample.
    pub sample: StatisticsConfig,
}

/// The per-connection request gates of one pass.
type Gates = Arc<Mutex<BTreeMap<CatalogConnectionId, Arc<Mutex<()>>>>>;

/// What one sampling pass did. Counts are diagnostics; the ledger holds the
/// samples.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SampleSummary {
    /// Samples recorded.
    pub sampled: u32,
    /// Tables whose metadata location was already sampled.
    pub unchanged: u32,
    /// Tables that could not be sampled, including staged tables.
    pub skipped: u32,
}

/// Why one table was not sampled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleOutcome {
    /// A new sample was recorded.
    Sampled,
    /// The metadata location was unchanged, so the counters are unchanged too.
    Unchanged,
    /// There is nothing to sample yet.
    Skipped(&'static str),
}

/// Why one table's sample failed.
#[derive(Debug, thiserror::Error)]
pub enum SampleError {
    /// The table, its catalog, or its storage could not be resolved.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// Reading metadata or manifests failed, or the table is too large.
    #[error(transparent)]
    Statistics(#[from] bergsmith_maintenance::StatisticsError),
    /// Reading or writing the ledger failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// Serializing the sample failed.
    #[error("serializing the sample failed")]
    Json(#[source] serde_json::Error),
}

impl From<serde_json::Error> for SampleError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl SampleError {
    /// A stable reason for operator logs.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Access(error) => error.reason_code(),
            Self::Statistics(_) => "TableStatisticsUnavailable",
            Self::Persistence(_) => "StatisticsLedgerUnavailable",
            Self::Json(_) => "TableStatisticsUnserializable",
        }
    }
}

/// Samples registered tables on a fixed interval.
pub struct StatisticsCollector {
    pool: PgPool,
    catalogs: Arc<RuntimeCatalogs>,
    settings: SamplingSettings,
    refresh: Arc<Semaphore>,
}

impl StatisticsCollector {
    /// Creates a collector. `settings` is validated by configuration parsing.
    #[must_use]
    pub fn new(pool: PgPool, catalogs: Arc<RuntimeCatalogs>, settings: SamplingSettings) -> Self {
        let refresh = Arc::new(Semaphore::new(settings.max_concurrent_connections.max(1)));
        Self {
            pool,
            catalogs,
            settings,
            refresh,
        }
    }

    /// The sampling bounds.
    #[must_use]
    pub const fn settings(&self) -> &SamplingSettings {
        &self.settings
    }

    /// Runs one pass immediately and then at `interval` until shutdown.
    pub async fn run(&self, interval: Duration, shutdown: impl Future<Output = ()> + Send) {
        tokio::pin!(shutdown);
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = &mut shutdown => return,
                _ = tick.tick() => {
                    if let Err(error) = self.pass().await {
                        warn!(
                            error = %error,
                            reason_code = "StatisticsLedgerUnavailable",
                            "statistics pass failed"
                        );
                    }
                }
            }
        }
    }

    /// Samples every due table at the current instant.
    ///
    /// # Errors
    ///
    /// A ledger failure that concerns the whole pass. A failure scoped to one
    /// table is logged and counted as skipped.
    pub async fn pass(&self) -> Result<SampleSummary, PersistenceError> {
        self.pass_at(Utc::now()).await
    }

    /// Samples every due table at `now`. The explicit instant keeps tests and
    /// replay diagnostics deterministic.
    ///
    /// # Errors
    ///
    /// As [`Self::pass`].
    pub async fn pass_at(&self, now: DateTime<Utc>) -> Result<SampleSummary, PersistenceError> {
        let mut summary = SampleSummary::default();
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let retention =
            TimeDelta::try_days(i64::try_from(self.settings.retention_days).unwrap_or(0))
                .unwrap_or(TimeDelta::MAX);
        statistics::prune(&mut conn, now - retention, PRUNE_BATCH).await?;
        statistics::prune_snapshots(&mut conn, now - retention, PRUNE_BATCH).await?;
        let cutoff = now - self.interval();
        let due = statistics::due_tables(&mut conn, cutoff, self.settings.page_size).await?;
        drop(conn);

        // Connections make progress independently, but a single connection
        // sees one catalog request at a time: its gate is held for the whole
        // attempt, including the fallback to another alias.
        let gates: Gates = Arc::new(Mutex::new(BTreeMap::new()));
        let concurrency = self.settings.max_concurrent_connections.max(1);
        let results: Vec<Result<SampleOutcome, SampleError>> = futures::stream::iter(due)
            .map(|table| {
                let gates = Arc::clone(&gates);
                async move { self.sample_due(&table, now, &gates).await }
            })
            .buffer_unordered(concurrency)
            .collect()
            .await;
        for result in results {
            match result {
                Ok(SampleOutcome::Sampled) => summary.sampled += 1,
                Ok(SampleOutcome::Unchanged) => summary.unchanged += 1,
                Ok(SampleOutcome::Skipped(reason)) => {
                    summary.skipped += 1;
                    debug!(reason, "table statistics sample skipped");
                }
                Err(error) => {
                    summary.skipped += 1;
                    warn!(
                        reason_code = error.reason_code(),
                        error = %error,
                        "table statistics sample failed"
                    );
                }
            }
        }
        Ok(summary)
    }

    /// Samples one table now, as after a publication, through the same alias
    /// fallback as the periodic pass.
    ///
    /// # Errors
    ///
    /// [`SampleError`] when the table cannot be resolved, read, or recorded.
    pub async fn sample_job(&self, job: &Job) -> Result<SampleOutcome, SampleError> {
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let identity = tables::table_identity(&mut conn, job.organization_id, job.table_id).await?;
        drop(conn);
        let table = DueTable {
            organization_id: job.organization_id,
            table_id: job.table_id,
            table_uuid: identity.table_uuid,
            last_confirmed_at: None,
        };
        // The publication path has no other request in flight for this table
        // beyond a concurrent pass, and the ledger keys samples by instant.
        self.sample_due(&table, Utc::now(), &Gates::default()).await
    }

    /// Requests an immediate sample for a published job without blocking the
    /// publication pass. A pass that is already refreshing tables drops the
    /// request; the interval samples the table anyway.
    pub fn request_refresh(self: &Arc<Self>, job: &Job) {
        let Ok(permit) = self.refresh.clone().try_acquire_owned() else {
            debug!("statistics refresh already running; the interval will cover the table");
            return;
        };
        let collector = Arc::clone(self);
        let job = job.clone();
        tokio::spawn(async move {
            match collector.sample_job(&job).await {
                Ok(SampleOutcome::Sampled) => {
                    debug!(job_id = %job.id, "published table sampled");
                }
                Ok(SampleOutcome::Unchanged) => {}
                Ok(SampleOutcome::Skipped(reason)) => {
                    debug!(job_id = %job.id, reason, "published table not sampled");
                }
                Err(error) => {
                    warn!(
                        job_id = %job.id,
                        reason_code = error.reason_code(),
                        error = %error,
                        "sampling the published table failed"
                    );
                }
            }
            drop(permit);
        });
    }

    /// The sampling interval as a `chrono` duration.
    fn interval(&self) -> TimeDelta {
        let millis = i64::try_from(self.settings.interval_millis).unwrap_or(i64::MAX);
        TimeDelta::try_milliseconds(millis).unwrap_or(TimeDelta::MAX)
    }

    /// Samples a due table at `at` through the first alias that can be read.
    async fn sample_due(
        &self,
        table: &DueTable,
        at: DateTime<Utc>,
        gates: &Gates,
    ) -> Result<SampleOutcome, SampleError> {
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let routes =
            statistics::maintenance_routes(&mut conn, table.organization_id, table.table_id)
                .await?;
        drop(conn);
        let mut last = None;
        for route in routes.into_iter().take(MAX_ROUTE_ATTEMPTS) {
            let gate = {
                let mut gates = gates.lock().await;
                Arc::clone(
                    gates
                        .entry(route.connection_id)
                        .or_insert_with(|| Arc::new(Mutex::new(()))),
                )
            };
            let _request = gate.lock().await;
            match self.sample_route(table, &route, at).await {
                Ok(outcome) => return Ok(outcome),
                Err(error) => last = Some(error),
            }
        }
        match last {
            Some(error) => Err(error),
            None => Ok(SampleOutcome::Skipped("no maintenance-enabled alias")),
        }
    }

    /// Resolves and samples one alias of a table.
    async fn sample_route(
        &self,
        table: &DueTable,
        route: &MaintenanceRoute,
        at: DateTime<Utc>,
    ) -> Result<SampleOutcome, SampleError> {
        let binding = self
            .catalogs
            .binding_for_connection(
                table.organization_id,
                route.connection_id,
                table.table_id,
                table.table_uuid,
                TableName {
                    namespace: route.namespace.clone(),
                    name: route.table_name.clone(),
                },
            )
            .await?;
        self.sample_binding(table.organization_id, table.table_id, &binding, at)
            .await
    }

    /// Samples an already resolved table and records the result.
    async fn sample_binding(
        &self,
        organization_id: OrganizationId,
        table_id: TableId,
        binding: &TableBinding,
        at: DateTime<Utc>,
    ) -> Result<SampleOutcome, SampleError> {
        let Some(metadata_location) = binding.loaded.metadata_location.clone() else {
            return Ok(SampleOutcome::Skipped("table is staged"));
        };
        {
            // Never hold this connection across the catalog and storage reads.
            let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
            let latest = statistics::latest(&mut conn, organization_id, table_id).await?;
            if latest.is_some_and(|sample| sample.metadata_location == metadata_location) {
                // The counters are unchanged, so there is no new sample. The
                // snapshot ledger can still be behind — a table sampled before
                // this ledger existed, or rows removed with retention — and
                // the metadata already loaded holds the history, so reconcile
                // it without another catalog read.
                let current = binding
                    .loaded
                    .metadata
                    .current_snapshot()
                    .map(|snapshot| snapshot.snapshot_id());
                let recorded = match current {
                    Some(snapshot_id) => {
                        statistics::has_snapshot(&mut conn, organization_id, table_id, snapshot_id)
                            .await?
                    }
                    None => true,
                };
                if !recorded {
                    let rows = snapshot_rows(binding, &self.settings.sample)?;
                    if window_holds(&rows, current) {
                        statistics::record_snapshots(
                            &mut conn,
                            organization_id,
                            table_id,
                            &rows,
                            at,
                        )
                        .await?;
                    }
                }
                // The counters are unchanged, but the table's freshness is
                // not: this pass read the metadata successfully.
                statistics::record_check(&mut conn, organization_id, table_id, at).await?;
                return Ok(SampleOutcome::Unchanged);
            }
        }
        let sample = TableSample::collect(
            &binding.loaded.metadata,
            &metadata_location,
            binding.store.file_io(),
            &self.settings.sample,
        )
        .await?;
        let new = NewTableSample {
            catalog_connection_id: binding.connection.id,
            sampled_at: at,
            metadata_location,
            snapshot_id: sample.snapshot_id,
            last_operation: sample.last_operation,
            record_count: sample.record_count,
            data_files: sample.data_files,
            data_bytes: sample.data_bytes,
            delete_files: sample.delete_files,
            delete_bytes: sample.delete_bytes,
            position_delete_files: sample.position_delete_files,
            equality_delete_files: sample.equality_delete_files,
            position_delete_records: sample.position_delete_records,
            equality_delete_records: sample.equality_delete_records,
            manifest_count: sample.manifest_count,
            data_manifest_count: sample.data_manifest_count,
            manifest_bytes: sample.manifest_bytes,
            partition_count: sample.partition_count,
            max_partition_files: sample.max_partition_files,
            small_files: sample.small_files,
            target_file_size_bytes: sample.target_file_size_bytes,
            snapshot_count: sample.snapshot_count,
            oldest_snapshot_at: sample.oldest_snapshot_at,
            newest_snapshot_at: sample.newest_snapshot_at,
            size_histogram: serde_json::to_value(&sample.size_histogram)?,
            top_partitions: serde_json::to_value(&sample.top_partitions)?,
        };
        let rows = snapshot_rows(binding, &self.settings.sample)?;
        // The sample and the commits it was taken with commit together, so a
        // failure cannot leave a sample whose ledger the next pass skips.
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let mut tx = conn.begin().await.map_err(PersistenceError::from)?;
        statistics::record(&mut tx, organization_id, table_id, &new).await?;
        statistics::record_check(&mut tx, organization_id, table_id, at).await?;
        statistics::record_snapshots(&mut tx, organization_id, table_id, &rows, at).await?;
        tx.commit().await.map_err(PersistenceError::from)?;
        Ok(SampleOutcome::Sampled)
    }
}

/// Whether the bounded window still holds `current`.
///
/// The ledger is a window of the newest snapshots, not a mirror of the
/// metadata: when the table's current snapshot falls outside it, its absence
/// from the ledger is retention rather than a gap, and no write can close it.
fn window_holds(rows: &[NewTableSnapshot], current: Option<i64>) -> bool {
    current.is_some_and(|snapshot_id| rows.iter().any(|row| row.snapshot_id == snapshot_id))
}

/// The snapshot ledger rows one loaded table contributes, newest first.
fn snapshot_rows(
    binding: &TableBinding,
    config: &StatisticsConfig,
) -> Result<Vec<NewTableSnapshot>, serde_json::Error> {
    snapshot_history(&binding.loaded.metadata, config.snapshot_history)
        .into_iter()
        .map(|entry| {
            Ok(NewTableSnapshot {
                catalog_connection_id: binding.connection.id,
                snapshot_id: entry.snapshot_id,
                parent_snapshot_id: entry.parent_snapshot_id,
                sequence_number: entry.sequence_number,
                committed_at: entry.committed_at,
                operation: entry.operation,
                summary: serde_json::to_value(entry.properties)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn row(snapshot_id: i64, committed_at_ms: i64) -> NewTableSnapshot {
        NewTableSnapshot {
            catalog_connection_id: CatalogConnectionId::from_uuid(Uuid::nil()),
            snapshot_id,
            parent_snapshot_id: None,
            sequence_number: snapshot_id,
            committed_at: DateTime::from_timestamp_millis(committed_at_ms)
                .expect("a fixed instant"),
            operation: "append".to_owned(),
            summary: serde_json::json!({}),
        }
    }

    #[test]
    fn a_rolled_back_table_holds_its_current_snapshot() {
        // A rollback leaves the newer commit recorded; the current snapshot is
        // the older member, not the newest row.
        let rows = [row(2, 2_000), row(1, 1_000)];
        assert!(window_holds(&rows, Some(1)));
    }

    #[test]
    fn snapshots_sharing_an_instant_are_both_in_the_window() {
        // Snapshot IDs do not encode commit order, so membership alone decides.
        let rows = [row(7, 1_000), row(6, 1_000)];
        assert!(window_holds(&rows, Some(6)));
        assert!(window_holds(&rows, Some(7)));
    }

    #[test]
    fn a_snapshot_outside_the_window_is_not_expected() {
        let rows = [row(3, 3_000), row(2, 2_000)];
        assert!(!window_holds(&rows, Some(1)));
        assert!(!window_holds(&rows, None));
    }
}
