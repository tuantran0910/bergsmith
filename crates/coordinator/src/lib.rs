//! The Bergsmith coordinator: public API, admission, scheduling, leases,
//! reconciliation, and publication.
//!
//! Services are implemented from BS-012 onward. This crate is the only one
//! allowed to depend on `bergsmith-persistence` and `bergsmith-catalog`.

pub mod database;
pub mod inventory;
pub mod planning;
pub mod publication;
