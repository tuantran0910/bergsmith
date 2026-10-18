//! The Bergsmith worker: registration, task polling, heartbeats, subprocess
//! supervision, and result reporting.
//!
//! Local execution is available without a coordinator. Distributed execution
//! isolates native work while the supervisor maintains leases and reporting.

pub mod local;
pub mod supervision;
