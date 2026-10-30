//! Shared Bergsmith domain types.
//!
//! This crate holds the typed identifiers, lifecycle states, transition rules,
//! failure dispositions, and scheduling configuration defined in
//! `.private/BERGSMITH_PROJECT_PLAN.md` §2 and §5. Every other crate depends on
//! it, so it performs no I/O and contains no async code.

pub mod audit;
pub mod capabilities;
pub mod digest;
pub mod disposition;
pub mod health;
pub mod ids;
pub mod job;
pub mod notifications;
pub mod permission;
pub mod policy;
pub mod schedule;
pub mod search;
pub mod secret;
pub mod settings;
pub mod table;
pub mod task;
pub mod views;
pub mod window;
