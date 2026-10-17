//! Planner configuration.

use serde::{Deserialize, Serialize};

use crate::error::PlanError;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Limits and thresholds for planning one job.
///
/// The defaults are development defaults from the plan, not performance
/// guarantees. A plan records the configuration it was built with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannerConfig {
    /// Target size of each replacement file.
    pub target_file_size_bytes: u64,
    /// Files smaller than this are compaction candidates.
    pub min_file_size_bytes: u64,
    /// The most input bytes one task (group) rewrites. Larger files are
    /// never selected.
    pub max_group_input_bytes: u64,
    /// The most live manifest entries planning reads before refusing the
    /// table, which bounds planner memory.
    pub max_manifest_entries: u64,
    /// The most input bytes one job rewrites.
    pub max_job_input_bytes: u64,
    /// The most source files one job rewrites.
    pub max_job_files: u64,
    /// How many tasks of one job may run at once.
    pub max_concurrent_groups: u32,
    /// A bin without deletes needs at least this many files.
    pub min_input_files: u64,
    /// A bin without deletes must remove at least this many files.
    pub min_files_removed: u64,
    /// Expected rewrite throughput of one task. BS-010 replaces the
    /// default with a measured value.
    pub throughput_bytes_per_second: u64,
    /// Fixed cost of one task: scheduling, opening files, and closing
    /// writers.
    pub task_overhead_millis: u64,
    /// The longest a job is expected to take, so it fits between two
    /// commits of a streaming writer (ADR 0010).
    pub duration_budget_millis: u64,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        let target = 256 * MIB;
        Self {
            target_file_size_bytes: target,
            min_file_size_bytes: target / 4 * 3,
            max_group_input_bytes: 2 * GIB,
            max_manifest_entries: 1_000_000,
            max_job_input_bytes: 100 * GIB,
            max_job_files: 10_000,
            max_concurrent_groups: 3,
            min_input_files: 5,
            min_files_removed: 2,
            throughput_bytes_per_second: 64 * MIB,
            task_overhead_millis: 2_000,
            duration_budget_millis: 180_000,
        }
    }
}

impl PlannerConfig {
    /// Checks that the limits are consistent.
    ///
    /// # Errors
    ///
    /// [`PlanError::InvalidConfig`] naming the first inconsistent limit.
    pub fn validate(&self) -> Result<(), PlanError> {
        let invalid = |message: &str| Err(PlanError::InvalidConfig(message.to_owned()));
        if self.target_file_size_bytes == 0 {
            return invalid("the target file size must be positive");
        }
        if self.min_file_size_bytes > self.target_file_size_bytes {
            return invalid("the minimum file size must not exceed the target");
        }
        if self.max_group_input_bytes < self.target_file_size_bytes {
            return invalid("the group input limit must be at least the target file size");
        }
        if self.max_concurrent_groups == 0 {
            return invalid("at least one group must be able to run");
        }
        if self.throughput_bytes_per_second == 0 {
            return invalid("the throughput must be positive");
        }
        Ok(())
    }
}
