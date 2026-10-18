//! The Bergsmith coordinator: public API, admission, scheduling, leases,
//! reconciliation, and publication.
//!
//! Durable publication and worker RPCs are implemented in BS-012–BS-013;
//! automatic scheduling and process supervision follow in BS-014–BS-015.
//! This crate is the only one allowed to depend on `bergsmith-persistence`
//! and `bergsmith-catalog`.

pub mod database;
pub mod durable_publication;
pub mod inventory;
pub mod planning;
pub mod publication;
pub mod results;
pub mod worker_access;
pub mod workers;
