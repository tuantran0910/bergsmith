//! Iceberg REST catalog integration: connections, capability discovery,
//! vended credentials, and the conditional commit adapter.
//!
//! Only the coordinator depends on this crate; workers have no catalog client.
//! Implemented in BS-005 and BS-006.
