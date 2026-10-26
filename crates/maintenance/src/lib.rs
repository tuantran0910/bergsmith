//! Native Iceberg maintenance: deterministic planning, the rewrite executor, and
//! delete application on iceberg-rust.
//!
//! The planner (BS-009) reads table metadata and manifests, never rows. The
//! executor verifies planned scans, streams native rewrites into attempt roots,
//! and produces immutable receipts (BS-010). Extended delete certification and
//! publication manifest tests follow in BS-011.

mod align;
mod baseline;
mod binpack;
mod budget;
mod config;
pub mod deadline;
mod definition;
mod deletes;
mod eligibility;
mod error;
mod execution_io;
mod executor;
mod hex;
mod inventory;
mod limits;
mod partition;
mod plan;
mod planner;
mod rewrite;
mod selection;
mod statistics;

pub use align::{
    AlignedStart, Alignment, AlignmentConfig, HeadObserver, ObservedHead, align_start,
};
pub use baseline::PlanningBaseline;
pub use budget::{BinDisposition, DeferReason, Estimate};
pub use config::PlannerConfig;
pub use definition::{
    DEFINITION_PROPERTIES, FieldDefinition, SchemaDefinition, SortFieldDefinition,
    SortOrderDefinition, SpecDefinition, SpecFieldDefinition, TableDefinition,
};
pub use eligibility::{KeyProblem, Rejection, SkipReason};
pub use error::PlanError;
pub use executor::{ExecutionInput, Executor, ExecutorError, NativeExecutor, PreparedScan};
pub use limits::{ExecutionLimits, ResourceLimit};
pub use partition::{PartitionKey, PartitionValue};
pub use plan::{PlanDocument, PlanIds, PlannedDelete, PlannedFile, PlannedTask};
pub use planner::{BinPreview, FileTally, NoOpReason, PlanInput, PlanOutcome, Planner, Preview};
pub use rewrite::{AttemptContext, ExecutionRequest, ExecutionResult};
pub use selection::PartitionPredicate;
pub use statistics::{
    PartitionFiles, SizeBucket, SnapshotDeltas, SnapshotEntry, StatisticsConfig, StatisticsError,
    TableSample, snapshot_history,
};
