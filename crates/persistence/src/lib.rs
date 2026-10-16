//! PostgreSQL repositories and migrations for the durable execution ledger.
//!
//! Only the coordinator depends on this crate; workers never touch the
//! database. Repositories keep each transaction short and never hold one
//! across network, gRPC, or object-store I/O.

mod database;
mod error;

pub use database::{Database, DatabaseConfig, DatabaseUrl, MIGRATOR};
pub use error::PersistenceError;
