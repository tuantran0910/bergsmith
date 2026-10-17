//! Native Iceberg maintenance: deterministic planning, the rewrite executor, and
//! delete application on iceberg-rust.
//!
//! The planner (BS-009) reads table metadata and manifests, never rows. The
//! executor and delete application follow in BS-010 and BS-011.

mod baseline;
mod binpack;
mod budget;
mod config;
mod deletes;
mod eligibility;
mod error;
mod hex;
mod inventory;
mod partition;
mod planner;
mod selection;

pub use baseline::PlanningBaseline;
pub use budget::{BinDisposition, DeferReason, Estimate};
pub use config::PlannerConfig;
pub use eligibility::{KeyProblem, Rejection, SkipReason};
pub use error::PlanError;
pub use partition::{PartitionKey, PartitionValue};
pub use planner::{BinPreview, FileTally, PlanInput, Planner, Preview};
pub use selection::PartitionPredicate;
