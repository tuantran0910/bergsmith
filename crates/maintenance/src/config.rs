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
}

impl Default for PlannerConfig {
    fn default() -> Self {
        let target = 256 * MIB;
        Self {
            target_file_size_bytes: target,
            min_file_size_bytes: target / 4 * 3,
            max_group_input_bytes: 2 * GIB,
            max_manifest_entries: 1_000_000,
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
        Ok(())
    }
}
