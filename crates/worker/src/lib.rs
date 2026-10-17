//! The Bergsmith worker: registration, task polling, heartbeats, subprocess
//! supervision, and result reporting.
//!
//! Local execution is available without a coordinator. Distributed supervision
//! follows in BS-013 and BS-015.

pub mod local;
