//! Shared Bergsmith domain types.
//!
//! This crate holds the typed identifiers, lifecycle states, transition rules,
//! and failure dispositions defined in `.private/BERGSMITH_PROJECT_PLAN.md` §2. Every
//! other crate depends on it, so it performs no I/O and contains no async code.

pub mod digest;
pub mod disposition;
pub mod ids;
pub mod job;
pub mod secret;
pub mod task;
