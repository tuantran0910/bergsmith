//! Native Iceberg v2 fixtures and an independent live-row oracle (ADR 0016).
//!
//! Dev-only: only test and bench targets depend on this crate.
//!
//! - [`TableSpec`] declares a table as ordered commits of data,
//!   position-delete, and equality-delete files. [`TableSpec::generate`]
//!   writes it as real Parquet, manifests, manifest lists, and
//!   `metadata.json` on the local filesystem.
//! - [`live_rows`] computes the multiset of live rows from those files with
//!   its own reader and delete rules, independently of iceberg-rust's reader
//!   and delete filter.

mod declare;
mod encode;
mod error;
mod generate;
mod oracle;
mod value;

pub use declare::{Commit, FileSpec, TableSpec};
pub use error::{FixtureError, Result};
pub use generate::Fixture;
pub use oracle::live_rows;
pub use value::{Row, RowMultiset, Value};
