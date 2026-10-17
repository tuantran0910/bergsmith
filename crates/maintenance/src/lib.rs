//! Native Iceberg maintenance: deterministic planning, the rewrite executor, and
//! delete application on iceberg-rust.
//!
//! The planner (BS-009) reads table metadata and manifests, never rows. The
//! executor and delete application follow in BS-010 and BS-011.

mod baseline;
mod error;

pub use baseline::PlanningBaseline;
pub use error::PlanError;
