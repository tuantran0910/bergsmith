//! What a maintenance policy asks the coordinator to do (BS-019.T1, BS-019.T3).
//!
//! A policy is the reusable "what" of a scheduled run: the operation, its
//! bounded parameters, and the most input one run may rewrite. It does not name
//! a table, a time, or a service identity; a schedule binds a policy to those.
//! The allowed start window belongs to the schedule, not the policy, because it
//! is a property of when the table is quiet.

use std::fmt;

/// The maintenance operation a policy runs.
///
/// The MVP implements binpack compaction only (plan §4 "MVP scope summary").
/// The enum exists so persisted rows can name an operation without a free-form
/// string, and so adding one is an explicit decision rather than a typo.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MaintenanceOperation {
    /// Binpack small data files into fewer, larger ones.
    #[default]
    Compact,
}

impl MaintenanceOperation {
    /// Every operation, in a stable order.
    pub const ALL: &'static [Self] = &[Self::Compact];

    /// The stable name persisted in PostgreSQL and used by the API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
        }
    }

    /// Parses the persisted name, or `None` if it is unrecognized.
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|operation| operation.as_str() == value)
    }
}

impl fmt::Display for MaintenanceOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a policy specification was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// The target file size was outside the supported range.
    #[error("target file size must be between {min} and {max} bytes, not {bytes}")]
    TargetFileSize {
        /// The rejected value.
        bytes: u64,
        /// Smallest accepted value.
        min: u64,
        /// Largest accepted value.
        max: u64,
    },
    /// Fewer input files than a compaction can usefully rewrite.
    #[error("a compaction needs between {min} and {max} input files, not {files}")]
    MinInputFiles {
        /// The rejected value.
        files: u32,
        /// Smallest accepted value.
        min: u32,
        /// Largest accepted value.
        max: u32,
    },
    /// The per-run input budget was outside the supported range.
    #[error("a run may rewrite between {min} and {max} input bytes, not {bytes}")]
    MaxInputBytes {
        /// The rejected value.
        bytes: u64,
        /// Smallest accepted value.
        min: u64,
        /// Largest accepted value.
        max: u64,
    },
    /// The per-run input budget was smaller than one replacement file.
    #[error("a run budget of {bytes} bytes cannot produce a {target}-byte file")]
    MaxInputBelowTarget {
        /// The rejected value.
        bytes: u64,
        /// The policy's target file size.
        target: u64,
    },
}

/// Bounded parameters for one maintenance operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicySpec {
    /// What to run.
    pub operation: MaintenanceOperation,
    /// Target size of each replacement file.
    pub target_file_size_bytes: u64,
    /// A bin without deletes needs at least this many input files.
    pub min_input_files: u32,
    /// The most input bytes one run may rewrite.
    ///
    /// This is the per-run budget: a scheduled job is planned with its policy's
    /// value, capped by the coordinator's global admission limit, so one run
    /// stays inside the table's commit cadence (plan §1).
    pub max_input_bytes: u64,
}

impl PolicySpec {
    /// Smallest accepted target file size: 1 MiB.
    pub const MIN_TARGET_FILE_SIZE_BYTES: u64 = 1024 * 1024;
    /// Largest accepted target file size: 1 GiB.
    pub const MAX_TARGET_FILE_SIZE_BYTES: u64 = 1024 * 1024 * 1024;
    /// Smallest accepted input-file count.
    pub const MIN_INPUT_FILES: u32 = 2;
    /// Largest accepted input-file count, so one policy cannot name a job the
    /// planner would refuse anyway.
    pub const MAX_INPUT_FILES: u32 = 1_000;
    /// Smallest accepted per-run input budget: 1 MiB.
    pub const MIN_RUN_INPUT_BYTES: u64 = 1024 * 1024;
    /// Largest accepted per-run input budget: 100 GiB, the plan's default
    /// maximum input for one job.
    pub const MAX_RUN_INPUT_BYTES: u64 = 100 * 1024 * 1024 * 1024;

    /// Validates a policy's parameters.
    ///
    /// # Errors
    ///
    /// [`PolicyError::TargetFileSize`], [`PolicyError::MinInputFiles`], or
    /// [`PolicyError::MaxInputBytes`] when a value is outside its supported
    /// range, and [`PolicyError::MaxInputBelowTarget`] when the run budget is
    /// smaller than one replacement file.
    pub const fn new(
        operation: MaintenanceOperation,
        target_file_size_bytes: u64,
        min_input_files: u32,
        max_input_bytes: u64,
    ) -> Result<Self, PolicyError> {
        if target_file_size_bytes < Self::MIN_TARGET_FILE_SIZE_BYTES
            || target_file_size_bytes > Self::MAX_TARGET_FILE_SIZE_BYTES
        {
            return Err(PolicyError::TargetFileSize {
                bytes: target_file_size_bytes,
                min: Self::MIN_TARGET_FILE_SIZE_BYTES,
                max: Self::MAX_TARGET_FILE_SIZE_BYTES,
            });
        }
        if min_input_files < Self::MIN_INPUT_FILES || min_input_files > Self::MAX_INPUT_FILES {
            return Err(PolicyError::MinInputFiles {
                files: min_input_files,
                min: Self::MIN_INPUT_FILES,
                max: Self::MAX_INPUT_FILES,
            });
        }
        if max_input_bytes < Self::MIN_RUN_INPUT_BYTES
            || max_input_bytes > Self::MAX_RUN_INPUT_BYTES
        {
            return Err(PolicyError::MaxInputBytes {
                bytes: max_input_bytes,
                min: Self::MIN_RUN_INPUT_BYTES,
                max: Self::MAX_RUN_INPUT_BYTES,
            });
        }
        if max_input_bytes < target_file_size_bytes {
            return Err(PolicyError::MaxInputBelowTarget {
                bytes: max_input_bytes,
                target: target_file_size_bytes,
            });
        }
        Ok(Self {
            operation,
            target_file_size_bytes,
            min_input_files,
            max_input_bytes,
        })
    }
}

impl Default for PolicySpec {
    /// Compaction at the planner's default target, which rewrites at least
    /// five files, with the planner's default maximum job input.
    ///
    /// These values mirror `bergsmith_maintenance::PlannerConfig::default`;
    /// keep them in step so a policy that names no parameters behaves like a
    /// manual run with the default configuration.
    fn default() -> Self {
        Self {
            operation: MaintenanceOperation::Compact,
            target_file_size_bytes: 256 * 1024 * 1024,
            min_input_files: 5,
            max_input_bytes: Self::MAX_RUN_INPUT_BYTES,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_names_round_trip_and_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for operation in MaintenanceOperation::ALL {
            assert!(seen.insert(operation.as_str()));
            assert_eq!(
                MaintenanceOperation::from_name(operation.as_str()),
                Some(*operation)
            );
        }
        assert_eq!(MaintenanceOperation::from_name("Expire"), None);
    }

    #[test]
    fn defaults_are_valid() {
        let spec = PolicySpec::default();
        assert_eq!(
            PolicySpec::new(
                spec.operation,
                spec.target_file_size_bytes,
                spec.min_input_files,
                spec.max_input_bytes
            ),
            Ok(spec)
        );
    }

    #[test]
    fn defaults_match_the_planner_defaults() {
        let spec = PolicySpec::default();
        assert_eq!(spec.target_file_size_bytes, 256 * 1024 * 1024);
        assert_eq!(spec.min_input_files, 5);
        assert_eq!(spec.max_input_bytes, 100 * 1024 * 1024 * 1024);
    }

    #[test]
    fn target_file_size_is_bounded() {
        assert!(matches!(
            PolicySpec::new(
                MaintenanceOperation::Compact,
                0,
                5,
                PolicySpec::MAX_RUN_INPUT_BYTES
            ),
            Err(PolicyError::TargetFileSize { .. })
        ));
        assert!(matches!(
            PolicySpec::new(
                MaintenanceOperation::Compact,
                PolicySpec::MIN_TARGET_FILE_SIZE_BYTES - 1,
                5,
                PolicySpec::MAX_RUN_INPUT_BYTES
            ),
            Err(PolicyError::TargetFileSize { .. })
        ));
        assert!(matches!(
            PolicySpec::new(
                MaintenanceOperation::Compact,
                PolicySpec::MAX_TARGET_FILE_SIZE_BYTES + 1,
                5,
                PolicySpec::MAX_RUN_INPUT_BYTES
            ),
            Err(PolicyError::TargetFileSize { .. })
        ));
        assert!(
            PolicySpec::new(
                MaintenanceOperation::Compact,
                PolicySpec::MIN_TARGET_FILE_SIZE_BYTES,
                5,
                PolicySpec::MAX_RUN_INPUT_BYTES
            )
            .is_ok()
        );
        assert!(
            PolicySpec::new(
                MaintenanceOperation::Compact,
                PolicySpec::MAX_TARGET_FILE_SIZE_BYTES,
                5,
                PolicySpec::MAX_RUN_INPUT_BYTES
            )
            .is_ok()
        );
    }

    #[test]
    fn input_file_count_is_bounded() {
        for files in [0, 1, PolicySpec::MAX_INPUT_FILES + 1] {
            assert!(matches!(
                PolicySpec::new(
                    MaintenanceOperation::Compact,
                    1024 * 1024,
                    files,
                    PolicySpec::MAX_RUN_INPUT_BYTES
                ),
                Err(PolicyError::MinInputFiles { .. })
            ));
        }
        assert!(
            PolicySpec::new(
                MaintenanceOperation::Compact,
                1024 * 1024,
                PolicySpec::MIN_INPUT_FILES,
                PolicySpec::MAX_RUN_INPUT_BYTES
            )
            .is_ok()
        );
    }

    #[test]
    fn the_per_run_input_budget_is_bounded() {
        for bytes in [
            0,
            PolicySpec::MIN_RUN_INPUT_BYTES - 1,
            PolicySpec::MAX_RUN_INPUT_BYTES + 1,
        ] {
            assert!(matches!(
                PolicySpec::new(MaintenanceOperation::Compact, 1024 * 1024, 5, bytes),
                Err(PolicyError::MaxInputBytes { .. })
            ));
        }
        for bytes in [
            PolicySpec::MIN_RUN_INPUT_BYTES,
            PolicySpec::MAX_RUN_INPUT_BYTES,
        ] {
            assert!(
                PolicySpec::new(MaintenanceOperation::Compact, 1024 * 1024, 5, bytes).is_ok(),
                "{bytes} bytes should be accepted"
            );
        }
    }

    #[test]
    fn the_per_run_input_budget_must_fit_one_replacement_file() {
        let target = 2 * 1024 * 1024;
        assert_eq!(
            PolicySpec::new(MaintenanceOperation::Compact, target, 5, target - 1),
            Err(PolicyError::MaxInputBelowTarget {
                bytes: target - 1,
                target,
            })
        );
        assert!(
            PolicySpec::new(MaintenanceOperation::Compact, target, 5, target).is_ok(),
            "a budget equal to the target can still write one file"
        );
    }
}
