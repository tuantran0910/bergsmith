//! Errors from generating fixtures and computing live rows.

use arrow_schema::ArrowError;
use parquet::errors::ParquetError;

/// Why a fixture could not be generated or its live rows computed.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// The fixture declaration is inconsistent, for example a value of the
    /// wrong type or a position delete naming an unknown data file.
    #[error("invalid fixture declaration: {0}")]
    Declaration(String),
    /// The table uses something the oracle does not support, such as a
    /// float equality key or a non-Parquet file.
    #[error("unsupported by the oracle: {0}")]
    Unsupported(String),
    /// The table's metadata or files contradict each other.
    #[error("invalid table content: {0}")]
    Content(String),
    /// iceberg-rust failed to build, write, or read metadata.
    #[error("iceberg operation failed")]
    Iceberg(#[from] iceberg::Error),
    /// Writing or reading Parquet failed.
    #[error("parquet operation failed")]
    Parquet(#[from] ParquetError),
    /// Building an Arrow batch failed.
    #[error("arrow operation failed")]
    Arrow(#[from] ArrowError),
    /// Creating or resolving the fixture directory failed.
    #[error("fixture directory operation failed")]
    Io(#[from] std::io::Error),
    /// Serializing table metadata failed.
    #[error("table metadata serialization failed")]
    Json(#[from] serde_json::Error),
}

/// Shorthand for results in this crate.
pub type Result<T, E = FixtureError> = std::result::Result<T, E>;
