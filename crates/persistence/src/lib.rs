//! PostgreSQL repositories and migrations for the durable execution ledger.
//!
//! Only the coordinator depends on this crate; workers never touch the database.
//! Implemented in BS-003.
