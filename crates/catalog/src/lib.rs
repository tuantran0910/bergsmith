//! Iceberg REST catalog integration: connections, capability discovery,
//! table loading with vended credentials, and the conditional commit
//! adapter.
//!
//! Bergsmith owns its catalog HTTP calls (ADR 0013) and reuses
//! iceberg-rust's REST wire types. Only the coordinator depends on this
//! crate; workers have no catalog client.

mod client;
mod commit;
mod connection;
mod error;

pub use client::{Auth, ConnectionConfig};
pub use commit::*;
pub use connection::{
    CapabilityReport, CatalogConnection, Listing, LoadedTable, MAX_LISTING_BYTES,
    MAX_LISTING_ITEMS, ObservedTable, TableName,
};
pub use error::{CatalogError, CatalogErrorModel};
