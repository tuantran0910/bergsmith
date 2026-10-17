//! Planner errors.

use bergsmith_storage::LocationError;

/// Why a plan could not be built.
///
/// Tables the MVP does not support are not errors: they produce preview
/// rejections. These errors mean planning itself could not finish.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// The planner configuration is inconsistent.
    #[error("invalid planner configuration: {0}")]
    InvalidConfig(String),
    /// A partition predicate does not fit the table.
    #[error("invalid selection: {0}")]
    InvalidSelection(String),
    /// The table has no metadata file, so a plan could not pin it.
    #[error("table {table_uuid} is staged and has no metadata location")]
    StagedTable {
        /// The table's `table-uuid`.
        table_uuid: uuid::Uuid,
    },
    /// The table or data location is not a valid storage location.
    #[error("invalid table location: {0}")]
    Location(#[from] LocationError),
    /// The baseline snapshot is not in the metadata.
    #[error("snapshot {snapshot_id} is not in the table metadata")]
    MissingSnapshot {
        /// The baseline snapshot.
        snapshot_id: i64,
    },
    /// A partition spec's source columns are in no schema of the table.
    #[error("partition spec {spec_id} does not bind to any table schema")]
    UnboundSpec {
        /// The spec.
        spec_id: i32,
    },
    /// A file names a partition spec the metadata does not have.
    #[error("partition spec {spec_id} is not in the table metadata")]
    UnknownSpec {
        /// The spec.
        spec_id: i32,
    },
    /// A file's partition tuple does not fit its spec.
    #[error("a partition tuple does not match partition spec {spec_id}")]
    PartitionMismatch {
        /// The spec.
        spec_id: i32,
    },
    /// A manifest entry has no data sequence number after inheritance.
    #[error("manifest entry for {path} has no sequence number")]
    MissingSequenceNumber {
        /// The file.
        path: String,
    },
    /// A path is live more than once in the snapshot.
    #[error("{path} is live more than once in the snapshot")]
    DuplicateFile {
        /// The file.
        path: String,
    },
    /// The snapshot has more live files than planning reads.
    #[error("the snapshot has more than {limit} live files")]
    InventoryTooLarge {
        /// The configured limit.
        limit: u64,
    },
    /// Reading or parsing metadata, a manifest list, or a manifest failed.
    #[error("reading table metadata failed: {0}")]
    Iceberg(#[from] iceberg::Error),
}
