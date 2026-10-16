//! Native Iceberg v2 fixtures (ADR 0016).
//!
//! Dev-only: only test and bench targets depend on this crate.
//!
//! [`TableSpec`] declares a table as ordered commits of data,
//! position-delete, and equality-delete files. [`TableSpec::generate`]
//! writes it as real Parquet, manifests, manifest lists, and
//! `metadata.json` on the local filesystem.

mod declare;
mod encode;
mod error;
mod generate;
mod value;

pub use declare::{Commit, FileSpec, TableSpec};
pub use error::{FixtureError, Result};
pub use generate::Fixture;
pub use value::{Row, RowMultiset, Value};
