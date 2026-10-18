//! PostgreSQL repositories and migrations for the durable execution ledger.
//!
//! Only the coordinator depends on this crate; workers never touch the
//! database. Repository functions take a `&mut PgConnection`, so callers can
//! compose them inside one transaction. Functions that must be atomic open
//! their own transaction (a savepoint when one is already open). No
//! transaction is ever held across network, gRPC, or object-store I/O.

pub mod attempts;
pub mod cancellation;
mod database;
mod error;
pub mod idempotency;
pub mod jobs;
pub mod leadership;
pub mod plans;
pub mod publications;
pub mod scheduling;
pub mod tables;
pub mod tenancy;
pub mod workers;

pub use database::{Database, DatabaseConfig, DatabaseUrl, MIGRATOR};
pub use error::PersistenceError;
