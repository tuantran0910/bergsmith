//! The Bergsmith coordinator: public API, admission, scheduling, leases,
//! reconciliation, and publication.
//!
//! Durable publication, worker RPCs, and scheduling/reconciliation are implemented
//! in BS-012–BS-014; worker process supervision follows in BS-015.
//! This crate is the only one allowed to depend on `bergsmith-persistence`
//! and `bergsmith-catalog`.

pub mod api;
pub mod database;
pub mod durable_publication;
pub mod identity;
pub mod inventory;
pub mod planning;
pub mod preview;
pub mod publication;
pub mod results;
pub mod scheduling;
pub mod worker_access;
pub mod workers;
