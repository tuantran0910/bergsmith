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
mod expiration;
mod health;
mod hex;
mod inventory;
mod limits;
pub mod orphans;
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
pub use expiration::{
    EXPIRATION_PLAN_VERSION, ExpirationDecision, ExpirationError, ExpirationPlanDocument,
    ExpirationPolicy, RetentionReason, plan_expiration,
};
pub use health::{
    AssessmentState, Finding, HealthError, HealthState, Recommendation, RecommendationAction,
    Severity, TableHealth, ThresholdSource, evaluate, recommend,
};
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
