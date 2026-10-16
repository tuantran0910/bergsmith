//! Iceberg REST catalog integration: connections, capability discovery,
//! table loading with vended credentials, and (in BS-006) the conditional
//! commit adapter.
//!
//! Bergsmith owns its catalog HTTP calls (ADR 0013) and reuses
//! iceberg-rust's REST wire types. Only the coordinator depends on this
//! crate; workers have no catalog client.

mod client;
mod connection;
mod error;

pub use client::{Auth, ConnectionConfig};
pub use connection::{CapabilityReport, CatalogConnection, LoadedTable, ObservedTable, TableName};
pub use error::{CatalogError, CatalogErrorModel};
