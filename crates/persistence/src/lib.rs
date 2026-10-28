//! PostgreSQL repositories and migrations for the durable execution ledger.
//!
//! Only the coordinator depends on this crate; workers never touch the
//! database. Repository functions take a `&mut PgConnection`, so callers can
//! compose them inside one transaction. Functions that must be atomic open
//! their own transaction (a savepoint when one is already open). No
//! transaction is ever held across network, gRPC, or object-store I/O.

pub mod attempts;
pub mod audit;
pub mod auth;
pub mod cancellation;
pub mod capabilities;
mod database;
mod error;
pub mod expirations;
pub mod grants;
pub mod idempotency;
pub mod jobs;
pub mod leadership;
pub mod maintenance;
pub mod metadata_plans;
pub mod metrics;
pub mod notifications;
pub mod occurrences;
pub mod orphans;
pub mod overview;
pub mod plans;
pub mod publications;
pub mod schedules;
pub mod scheduling;
pub mod statistics;
pub mod tables;
pub mod tenancy;
pub mod worker_tokens;
pub mod workers;

pub use database::{Database, DatabaseConfig, DatabaseTlsMode, DatabaseUrl, MIGRATOR};
pub use error::PersistenceError;

/// Latest complete storage observations and scoped rollups.
pub mod storage_breakdown;
