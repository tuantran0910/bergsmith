//! Planner errors.

use bergsmith_storage::LocationError;

/// Why a plan could not be built.
///
/// Tables the MVP does not support are not errors: they produce preview
/// rejections. These errors mean planning itself could not finish.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// The table has no metadata file, so a plan could not pin it.
    #[error("table {table_uuid} is staged and has no metadata location")]
    StagedTable {
        /// The table's `table-uuid`.
        table_uuid: uuid::Uuid,
    },
    /// The table or data location is not a valid storage location.
    #[error("invalid table location: {0}")]
    Location(#[from] LocationError),
    /// Reading or parsing metadata, a manifest list, or a manifest failed.
    #[error("reading table metadata failed: {0}")]
    Iceberg(#[from] iceberg::Error),
}
