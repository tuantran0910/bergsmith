//! Separately budgeted leader-owned storage collection; GET never runs a scan.
use super::{RuntimeCatalogs, config::ConfigError};
use bergsmith_catalog::TableName;
use bergsmith_common::ids::CatalogConnectionId;
use bergsmith_common::ids::JobId;
use bergsmith_maintenance::{
    deadline::within,
    orphans::{OrphanPolicy, ReachabilityBounds},
    storage_breakdown,
};
use bergsmith_persistence::{
    PersistenceError, orphans as orphan_plans, statistics::DueTable, storage_breakdown as ledger,
};
use bergsmith_storage::Location;
use chrono::{TimeDelta, Utc};
use futures::StreamExt;
use serde::Deserialize;
use sqlx::PgPool;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;

/// Read-only collection bounds, independent of cleanup enablement.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageSettings {
    /// Enables background reporting; false by default.
    pub enabled: bool,
    /// Interval between checks, including failed checks.
    pub interval_seconds: u64,
    /// Tables examined per pass.
    pub page_size: i64,
    /// Maximum concurrent scans; each catalog is still serialized within a pass.
    pub max_concurrent_scans: usize,
    /// Maximum objects consumed from a listing.
    pub max_listed_objects: usize,
    /// Maximum distinct manifest lists (snapshots and unresolved intents).
    pub max_manifest_lists: usize,
    /// Maximum distinct manifests.
    pub max_manifests: usize,
    /// Maximum decoded entries.
    pub max_entries: usize,
    /// Maximum size of a single metadata object read.
    pub max_object_bytes: u64,
    /// Maximum cumulative manifest/list bytes read.
    pub max_total_bytes: u64,
    /// Deadline per manifest/list object read; listing uses the whole-scan deadline.
    pub read_timeout_seconds: u64,
    /// Deadline for a complete scan.
    pub scan_timeout_seconds: u64,
}
impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: 3600,
            page_size: 20,
            max_concurrent_scans: 1,
            max_listed_objects: 1_000_000,
            max_manifest_lists: 10_000,
            max_manifests: 100_000,
            max_entries: 500_000,
            max_object_bytes: 64 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
            read_timeout_seconds: 120,
            scan_timeout_seconds: 600,
        }
    }
}
impl StorageSettings {
    /// Checks startup bounds, including safe duration conversions.
    ///
    /// # Errors
    /// Returns a configuration error for unusable or excessive bounds.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(60..=86400).contains(&self.interval_seconds)
            || !(1..=1000).contains(&self.page_size)
            || !(1..=16).contains(&self.max_concurrent_scans)
            || !(1..=10_000_000).contains(&self.max_listed_objects)
            || !(1..=100_000).contains(&self.max_manifest_lists)
            || !(1..=1_000_000).contains(&self.max_manifests)
            || !(1..=10_000_000).contains(&self.max_entries)
            || !(1..=1024 * 1024 * 1024).contains(&self.max_object_bytes)
            || self.max_total_bytes < self.max_object_bytes
            || self.max_total_bytes > 100 * 1024 * 1024 * 1024
            || !(1..=3600).contains(&self.read_timeout_seconds)
            || !(1..=3600).contains(&self.scan_timeout_seconds)
        {
            return Err(ConfigError::Invalid(
                "storage_breakdown bounds are invalid".into(),
            ));
        }
        Ok(())
    }
    /// Cutoff for a healthy observation, allowing one collection interval of grace.
    /// Failed checks remain stale immediately, independently of sample age.
    #[must_use]
    pub fn fresh_after(&self, now: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
        now - TimeDelta::seconds(self.interval_seconds.saturating_mul(2).cast_signed())
    }

    fn bounds(&self) -> Result<ReachabilityBounds, bergsmith_maintenance::orphans::OrphanError> {
        ReachabilityBounds::new(
            self.max_manifests,
            self.max_entries,
            self.max_object_bytes,
            Duration::from_secs(self.read_timeout_seconds),
        )?
        .with_scan_limits(self.max_manifest_lists, self.max_total_bytes)
    }
}

/// Owns bounded background scans for the current leadership epoch.
pub struct StorageCollector {
    pool: PgPool,
    catalogs: Arc<RuntimeCatalogs>,
    settings: StorageSettings,
    minimum_age_seconds: u64,
    epoch: i64,
    gates: Mutex<BTreeMap<CatalogConnectionId, Arc<Mutex<()>>>>,
    pass_gate: Mutex<()>,
    decoders: Arc<tokio::sync::Semaphore>,
}
impl StorageCollector {
    /// Creates a collector from startup-validated settings and cleanup age policy.
    #[must_use]
    pub fn new(
        pool: PgPool,
        catalogs: Arc<RuntimeCatalogs>,
        settings: StorageSettings,
        minimum_age_seconds: u64,
        epoch: i64,
    ) -> Self {
        let decoders = Arc::new(tokio::sync::Semaphore::new(
            settings.max_concurrent_scans.max(1),
        ));
        Self {
            pool,
            catalogs,
            settings,
            minimum_age_seconds,
            epoch,
            gates: Mutex::default(),
            pass_gate: Mutex::default(),
            decoders,
        }
    }
    /// Checks due tables every minute; disabled reporting performs no work.
    pub async fn run(&self, shutdown: impl Future<Output = ()> + Send) {
        tokio::pin!(shutdown);
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = &mut shutdown => return,
                _ = tick.tick() => if let Err(error) = self.pass().await {
                    tracing::warn!(reason="StorageLedgerUnavailable", error=%error, "storage collection pass failed");
                }
            }
        }
    }
    /// Collects one bounded page. Failures are persisted without erasing samples.
    ///
    /// # Errors
    /// Returns a ledger or leadership error; per-table scan failures are recorded.
    pub async fn pass(&self) -> Result<(), PersistenceError> {
        if !self.settings.enabled {
            return Ok(());
        }
        let _pass = self.pass_gate.lock().await;
        let due = {
            let mut conn = self.pool.acquire().await?;
            if bergsmith_persistence::leadership::live_epoch(&mut conn).await? != Some(self.epoch) {
                return Err(PersistenceError::LeadershipLost { epoch: self.epoch });
            }
            ledger::due(
                &mut conn,
                Utc::now() - TimeDelta::seconds(self.settings.interval_seconds.cast_signed()),
                self.settings.page_size,
            )
            .await?
        };
        let results: Vec<_> = futures::stream::iter(due)
            .map(|table| async move { self.sample(&table).await })
            .buffer_unordered(self.settings.max_concurrent_scans)
            .collect()
            .await;
        for result in results {
            result?;
        }
        Ok(())
    }

    async fn sample(&self, table: &DueTable) -> Result<(), PersistenceError> {
        let result = self.scan(table).await;
        let mut conn = self.pool.acquire().await?;
        match result {
            Ok((metadata, snapshot, content)) => {
                ledger::record(
                    &mut conn,
                    self.epoch,
                    &ledger::Observation {
                        organization_id: table.organization_id,
                        table_id: table.table_id,
                        metadata_location: &metadata,
                        snapshot_id: snapshot,
                        content: &content,
                        at: Utc::now(),
                    },
                )
                .await?;
            }
            Err(reason) => {
                tracing::warn!(table_id=%table.table_id,reason,"storage collection failed");
                ledger::failure(
                    &mut conn,
                    self.epoch,
                    table.organization_id,
                    table.table_id,
                    Utc::now(),
                    reason,
                )
                .await?;
            }
        }
        Ok(())
    }
    async fn scan(
        &self,
        table: &DueTable,
    ) -> Result<(String, Option<i64>, serde_json::Value), &'static str> {
        let routes = {
            let mut conn = self
                .pool
                .acquire()
                .await
                .map_err(|_| "StorageLedgerUnavailable")?;
            ledger::routes(&mut conn, table.organization_id, table.table_id)
                .await
                .map_err(|_| "StorageLedgerUnavailable")?
        };
        let mut last = "StorageNoEnabledAlias";
        for route in routes {
            let gate = {
                let mut gates = self.gates.lock().await;
                gates
                    .entry(route.connection_id)
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone()
            };
            let result = scan_after_gate(
                &gate,
                Duration::from_secs(self.settings.scan_timeout_seconds),
                async {
                    // Refresh ledger protections after queueing, before catalog
                    // metadata; no connection survives into the network reads.
                    let unresolved = {
                        let mut conn = self
                            .pool
                            .acquire()
                            .await
                            .map_err(|_| "StorageLedgerUnavailable")?;
                        orphan_plans::unresolved_manifest_lists(
                            &mut conn,
                            table.organization_id,
                            table.table_id,
                            JobId::generate(),
                        )
                        .await
                        .map_err(|_| "StorageLedgerUnavailable")?
                    };
                    let binding = self
                        .catalogs
                        .binding_for_connection(
                            table.organization_id,
                            route.connection_id,
                            table.table_id,
                            table.table_uuid,
                            TableName {
                                namespace: route.namespace,
                                name: route.table_name,
                            },
                        )
                        .await
                        .map_err(|_| "StorageCatalogUnavailable")?;
                    self.scan_binding(&binding, &unresolved).await
                },
            )
            .await
            .ok_or("StorageScanTimedOut")?;
            match result {
                Err("StorageCatalogUnavailable") => last = "StorageCatalogUnavailable",
                other => return other,
            }
        }
        Err(last)
    }

    async fn scan_binding(
        &self,
        binding: &super::catalogs::TableBinding,
        unresolved: &[String],
    ) -> Result<(String, Option<i64>, serde_json::Value), &'static str> {
        let location = binding
            .loaded
            .metadata_location
            .as_deref()
            .ok_or("StorageTableStaged")?;
        let root = Location::parse(binding.loaded.metadata.location())
            .map_err(|_| "StorageLocationInvalid")?;
        let policy = OrphanPolicy::new(self.minimum_age_seconds, binding.protection_window_ms(), 1)
            .map_err(|_| "StorageBoundsInvalid")?;
        let cutoff = chrono::DateTime::from_timestamp_millis(policy.cutoff_ms(Utc::now()))
            .ok_or("StorageBoundsInvalid")?;
        // Pagination shares the active whole-scan deadline; a million-object
        // listing is not one metadata-object read.
        let listed = binding
            .store
            .list_bounded(&root, self.settings.max_listed_objects)
            .await
            .map_err(|error| match error {
                bergsmith_storage::StorageError::ListingTooLarge { .. } => "StorageListingTooLarge",
                _ => "StorageListingUnavailable",
            })?;
        let scan = storage_breakdown::StorageScan::new(
            self.settings.bounds().map_err(|_| "StorageBoundsInvalid")?,
            self.decoders.clone(),
        );
        let sample = scan
            .collect(
                &binding.loaded.metadata,
                location,
                unresolved,
                binding.store.file_io(),
                &listed,
                cutoff,
            )
            .await
            .map_err(|error| reporting_reason(&error))?;
        let fresh = binding
            .catalog
            .load_table(&binding.table, false)
            .await
            .map_err(|_| "StorageBaselineUnavailable")?;
        if fresh.metadata.uuid() != binding.loaded.metadata.uuid()
            || fresh.metadata_location.as_deref() != Some(location)
        {
            return Err("StorageBaselineChanged");
        }
        let mut content = serde_json::to_value(sample).map_err(|_| "StorageUnserializable")?;
        content["version"] = serde_json::json!(1);
        Ok((
            location.to_owned(),
            binding
                .loaded
                .metadata
                .current_snapshot()
                .map(|snapshot| snapshot.snapshot_id()),
            content,
        ))
    }
}

// Queueing consumes no scan budget. Dropping a timed-out scan releases the
// catalog mutex so the next waiting table can start its own complete budget.
async fn scan_after_gate<T>(
    gate: &Mutex<()>,
    timeout: Duration,
    scan: impl Future<Output = T>,
) -> Option<T> {
    let _request = gate.lock().await;
    within(timeout, scan).await
}

fn reporting_reason(error: &bergsmith_maintenance::orphans::OrphanError) -> &'static str {
    use bergsmith_maintenance::orphans::OrphanError;
    match error {
        OrphanError::InvalidPolicy => "StorageBoundsInvalid",
        OrphanError::Location(_) => "StorageLocationInvalid",
        OrphanError::Io(_) => "StorageMetadataReadFailed",
        OrphanError::Storage(_) | OrphanError::Delete(_) => "StorageUnavailable",
        OrphanError::TimedOut { .. } => "StorageReadTimedOut",
        OrphanError::ScanTooLarge { .. } => "StorageManifestScanTooLarge",
        OrphanError::ListingTooLarge { .. } => "StorageListingTooLarge",
        OrphanError::ScanUnboundedManifest { .. } => "StorageManifestUnbounded",
        OrphanError::Blocking(_) => "StorageDecodeFailed",
        OrphanError::ScanObjectTooLarge { .. } => "StorageObjectTooLarge",
        OrphanError::ScanListsTooLarge { .. } => "StorageManifestListsExceeded",
        OrphanError::ScanBytesTooLarge { .. } => "StorageReadBudgetExceeded",
        OrphanError::InconsistentListing => "StorageListingInconsistent",
        OrphanError::InconsistentMetadata => "StorageMetadataInconsistent",
        OrphanError::AccountingOverflow => "StorageAccountingOverflow",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn storage_reporting_defaults_off_and_refuses_unbounded_settings() {
        let defaults = StorageSettings::default();
        assert!(!defaults.enabled);
        assert!(defaults.validate().is_ok());
        assert!(
            StorageSettings {
                max_concurrent_scans: 0,
                ..defaults.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            StorageSettings {
                max_entries: 0,
                ..defaults.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            StorageSettings {
                max_total_bytes: 1,
                ..defaults.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            StorageSettings {
                scan_timeout_seconds: 0,
                ..defaults
            }
            .validate()
            .is_err()
        );
    }
    #[tokio::test(start_paused = true)]
    async fn catalog_queue_wait_does_not_consume_the_scan_budget() {
        use futures::poll;
        let gate = Mutex::new(());
        let held = gate.lock().await;
        let work = scan_after_gate(&gate, Duration::from_secs(1), async { 7 });
        tokio::pin!(work);
        assert!(poll!(&mut work).is_pending());
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(poll!(&mut work).is_pending());
        drop(held);
        assert_eq!(work.await, Some(7));
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_scan_releases_its_catalog_for_the_next_table() {
        let gate = Mutex::new(());
        assert!(
            scan_after_gate(&gate, Duration::from_secs(1), std::future::pending::<()>())
                .await
                .is_none()
        );
        assert_eq!(
            scan_after_gate(&gate, Duration::from_secs(1), async { 9 }).await,
            Some(9)
        );
    }

    #[test]
    fn freshness_allows_one_interval_of_grace() {
        let settings = StorageSettings::default();
        let now = Utc::now();
        assert_eq!(now - settings.fresh_after(now), TimeDelta::hours(2));
    }

    #[test]
    fn reporting_failures_distinguish_limits_inconsistency_and_overflow() {
        use bergsmith_maintenance::orphans::OrphanError;
        let diagnostic = OrphanError::ScanListsTooLarge { limit: 7 }.diagnostics();
        assert_eq!(diagnostic.size, None, "a list count is not a byte size");
        assert_eq!(diagnostic.limit, Some(7));
        for (error, reason) in [
            (
                OrphanError::ScanListsTooLarge { limit: 1 },
                "StorageManifestListsExceeded",
            ),
            (
                OrphanError::ScanBytesTooLarge { limit: 1 },
                "StorageReadBudgetExceeded",
            ),
            (
                OrphanError::InconsistentListing,
                "StorageListingInconsistent",
            ),
            (
                OrphanError::InconsistentMetadata,
                "StorageMetadataInconsistent",
            ),
            (OrphanError::AccountingOverflow, "StorageAccountingOverflow"),
        ] {
            assert_eq!(reporting_reason(&error), reason);
        }
    }
}
