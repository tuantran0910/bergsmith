//! The Bergsmith coordinator: public API, admission, scheduling, leases,
//! reconciliation, and publication.
//!
//! Durable publication is implemented in BS-012; worker RPC orchestration and
//! automatic scheduling follow in BS-013–BS-015. This crate is the only one
//! allowed to depend on `bergsmith-persistence` and `bergsmith-catalog`.

pub mod database;
pub mod durable_publication;
pub mod inventory;
pub mod planning;
pub mod publication;
pub mod results;
