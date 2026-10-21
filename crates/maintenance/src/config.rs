//! Planner configuration.

use bergsmith_common::policy::PolicySpec;
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
    /// Applies a maintenance policy's parameters to a base configuration.
    ///
    /// A scheduled job is planned with the policy that admitted it:
    ///
    /// - its target file size and minimum input-file count replace the base
    ///   values;
    /// - the small-file threshold is lowered when necessary so a smaller
    ///   target still makes files eligible;
    /// - the per-run input budget caps both the job and the group limit, so no
    ///   single run can rewrite more than the policy allows.
    ///
    /// The base configuration keeps every other limit, including the global
    /// duration budget and manifest bound. `admission_max_input_bytes` is the
    /// coordinator's global admission cap; pass it here so the job and group
    /// limits stay ordered. The effective budget is the smallest of the
    /// policy, base, and admission values.
    ///
    /// Only [`bergsmith_common::policy::MaintenanceOperation::Compact`] exists
    /// in this release. A future operation must extend this mapping rather than
    /// silently inherit compaction's limits.
    ///
    /// # Errors
    ///
    /// [`PlanError::InvalidConfig`] when the result is inconsistent, for
    /// example when the admission cap cannot hold the policy's target file.
    pub fn for_policy(
        mut base: Self,
        policy: &PolicySpec,
        admission_max_input_bytes: u64,
    ) -> Result<Self, PlanError> {
        base.target_file_size_bytes = policy.target_file_size_bytes;
        base.min_file_size_bytes = base
            .min_file_size_bytes
            .min(policy.target_file_size_bytes / 4 * 3);
        base.min_input_files = u64::from(policy.min_input_files);
        base.max_job_input_bytes = base
            .max_job_input_bytes
            .min(policy.max_input_bytes)
            .min(admission_max_input_bytes);
        base.max_group_input_bytes = base.max_group_input_bytes.min(base.max_job_input_bytes);
        base.validate()?;
        Ok(base)
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use bergsmith_common::policy::{MaintenanceOperation, PolicySpec};

    #[test]
    fn the_default_policy_leaves_the_default_configuration_unchanged() {
        let config = PlannerConfig::for_policy(
            PlannerConfig::default(),
            &PolicySpec::default(),
            PlannerConfig::default().max_job_input_bytes,
        )
        .expect("the defaults are consistent");
        assert_eq!(config, PlannerConfig::default());
    }

    #[test]
    fn a_policy_replaces_the_target_and_minimum_file_count() {
        let policy = PolicySpec::new(
            MaintenanceOperation::Compact,
            512 * MIB,
            3,
            PlannerConfig::default().max_job_input_bytes,
        )
        .expect("a valid policy");
        let config = PlannerConfig::for_policy(
            PlannerConfig::default(),
            &policy,
            PlannerConfig::default().max_job_input_bytes,
        )
        .expect("a consistent configuration");
        assert_eq!(config.target_file_size_bytes, 512 * MIB);
        assert_eq!(config.min_input_files, 3);
        // The base threshold (192 MiB) is already below three quarters of the
        // new target, so it is kept rather than raised.
        assert_eq!(config.min_file_size_bytes, 192 * MIB);
    }

    #[test]
    fn a_smaller_target_lowers_the_small_file_threshold() {
        let policy = PolicySpec::new(
            MaintenanceOperation::Compact,
            MIB,
            2,
            PlannerConfig::default().max_job_input_bytes,
        )
        .expect("a valid policy");
        let config = PlannerConfig::for_policy(
            PlannerConfig::default(),
            &policy,
            PlannerConfig::default().max_job_input_bytes,
        )
        .expect("a consistent configuration");
        assert_eq!(config.target_file_size_bytes, MIB);
        assert_eq!(config.min_file_size_bytes, 768 * 1024);
    }

    #[test]
    fn the_run_budget_caps_the_job_and_group_limits() {
        let policy = PolicySpec::new(MaintenanceOperation::Compact, 256 * MIB, 5, 3 * GIB)
            .expect("a valid policy");
        let config = PlannerConfig::for_policy(
            PlannerConfig::default(),
            &policy,
            PlannerConfig::default().max_job_input_bytes,
        )
        .expect("a consistent configuration");
        assert_eq!(config.max_job_input_bytes, 3 * GIB);
        // The default group limit is 2 GiB, already below the run budget.
        assert_eq!(config.max_group_input_bytes, 2 * GIB);

        let tight = PolicySpec::new(MaintenanceOperation::Compact, 256 * MIB, 5, GIB)
            .expect("a valid policy");
        let config = PlannerConfig::for_policy(
            PlannerConfig::default(),
            &tight,
            PlannerConfig::default().max_job_input_bytes,
        )
        .expect("a consistent configuration");
        assert_eq!(config.max_job_input_bytes, GIB);
        assert_eq!(config.max_group_input_bytes, GIB);
    }

    #[test]
    fn the_admission_cap_is_a_third_minimum() {
        let policy = PolicySpec::new(MaintenanceOperation::Compact, 256 * MIB, 5, 8 * GIB)
            .expect("a valid policy");
        let config = PlannerConfig::for_policy(PlannerConfig::default(), &policy, 4 * GIB)
            .expect("a consistent configuration");
        assert_eq!(config.max_job_input_bytes, 4 * GIB);
        // The default group limit is already below the admission cap.
        assert_eq!(config.max_group_input_bytes, 2 * GIB);
    }

    #[test]
    fn an_admission_cap_below_the_target_is_rejected() {
        let policy = PolicySpec::new(MaintenanceOperation::Compact, GIB, 5, 4 * GIB)
            .expect("a valid policy");
        assert!(matches!(
            PlannerConfig::for_policy(PlannerConfig::default(), &policy, 512 * MIB),
            Err(PlanError::InvalidConfig(_))
        ));
    }

    #[test]
    fn an_inconsistent_result_is_rejected() {
        let base = PlannerConfig {
            max_job_input_bytes: 512 * MIB,
            ..PlannerConfig::default()
        };
        let policy = PolicySpec::new(
            MaintenanceOperation::Compact,
            GIB,
            5,
            PolicySpec::MAX_RUN_INPUT_BYTES,
        )
        .expect("a valid policy");
        assert!(matches!(
            PlannerConfig::for_policy(base, &policy, u64::MAX),
            Err(PlanError::InvalidConfig(_))
        ));
    }
}
