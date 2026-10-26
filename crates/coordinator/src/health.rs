//! Health inputs shared by the API and schedule admission (BS-028.T3).
//!
//! The model itself is pure and lives in `bergsmith-maintenance`; this module
//! only presents a ledger row as its input and reduces an evaluation to the
//! findings a policy trigger checks.

use bergsmith_common::health::{HealthFinding, HealthVerdict};
use bergsmith_maintenance::{TableHealth, TableSample};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::statistics::StoredTableSample;

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
