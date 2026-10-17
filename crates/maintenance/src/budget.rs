//! The benefit threshold, job limits, and duration budget (BS-009.T4).
//!
//! A bin is worth rewriting when it removes enough files or has deletes to
//! apply. Beneficial bins are admitted, most beneficial first, while the
//! job stays within its input limits and expected duration. The duration
//! estimate bounds any order in which the concurrent tasks could run, so the
//! scheduler's dispatch order cannot invalidate it.

use serde::{Deserialize, Serialize};

use crate::config::PlannerConfig;

/// What admission decided for a bin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinDisposition {
    /// The bin becomes a task.
    Admitted,
    /// Rewriting the bin would not remove enough files, and it has no
    /// deletes to apply.
    NoBenefit,
    /// The bin is beneficial but did not fit this job; a later job can
    /// take it.
    Deferred(DeferReason),
}

/// The job limit a deferred bin would have exceeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferReason {
    /// The maximum input bytes per job.
    JobInput,
    /// The maximum source files per job.
    JobFiles,
    /// The per-job duration budget.
    Duration,
}

/// The size of one bin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BinSize {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
    pub(crate) has_deletes: bool,
}

/// The admitted work and how long it is expected to take.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Estimate {
    /// Tasks admitted.
    pub tasks: u64,
    /// Data files they rewrite.
    pub files: u64,
    /// Bytes of those files.
    pub bytes: u64,
    /// Data files the job is expected to remove.
    pub files_removed: u64,
    /// Expected job duration in milliseconds, at most the budget.
    pub duration_millis: u64,
}

/// Files a bin is expected to remove: its files minus the replacement
/// files its bytes fill at the target size.
pub(crate) fn files_removed(bin: BinSize, config: &PlannerConfig) -> u64 {
    let outputs = bin.bytes.div_ceil(config.target_file_size_bytes).max(1);
    bin.files.saturating_sub(outputs)
}

fn beneficial(bin: BinSize, config: &PlannerConfig) -> bool {
    bin.has_deletes
        || (bin.files >= config.min_input_files
            && files_removed(bin, config) >= config.min_files_removed)
}

/// The expected run time of one task.
pub(crate) fn task_millis(bytes: u64, config: &PlannerConfig) -> u64 {
    let read = (u128::from(bytes) * 1000).div_ceil(u128::from(config.throughput_bytes_per_second));
    config
        .task_overhead_millis
        .saturating_add(u64::try_from(read).unwrap_or(u64::MAX))
}

/// Running totals of admitted work.
#[derive(Clone, Copy, Default)]
struct Totals {
    files: u64,
    bytes: u64,
    files_removed: u64,
    tasks: u64,
    task_millis: u64,
    longest_task_millis: u64,
}

impl Totals {
    fn with(self, bin: BinSize, config: &PlannerConfig) -> Self {
        let millis = task_millis(bin.bytes, config);
        Self {
            files: self.files + bin.files,
            bytes: self.bytes.saturating_add(bin.bytes),
            files_removed: self.files_removed + files_removed(bin, config),
            tasks: self.tasks + 1,
            task_millis: self.task_millis.saturating_add(millis),
            longest_task_millis: self.longest_task_millis.max(millis),
        }
    }

    /// Graham's bound for list scheduling on `slots` concurrent tasks:
    /// `total / slots + longest * (slots - 1) / slots`, which no dispatch
    /// order exceeds.
    fn duration_millis(&self, slots: u64) -> u64 {
        let spread = self.task_millis.div_ceil(slots);
        let tail = (u128::from(self.longest_task_millis) * u128::from(slots - 1))
            .div_ceil(u128::from(slots));
        spread.saturating_add(u64::try_from(tail).unwrap_or(u64::MAX))
    }
}

/// Decides each bin's disposition, in input order, and the resulting
/// estimate. `bins` must be in canonical order; ties in priority keep it.
pub(crate) fn admit(bins: &[BinSize], config: &PlannerConfig) -> (Vec<BinDisposition>, Estimate) {
    let mut dispositions = vec![BinDisposition::NoBenefit; bins.len()];
    let mut order: Vec<usize> = (0..bins.len())
        .filter(|&index| beneficial(bins[index], config))
        .collect();
    // Most files removed first, then the smaller bin; sort is stable.
    order.sort_by(|&a, &b| {
        let (a, b) = (bins[a], bins[b]);
        files_removed(b, config)
            .cmp(&files_removed(a, config))
            .then(a.bytes.cmp(&b.bytes))
    });
    let slots = u64::from(config.max_concurrent_groups);
    let mut totals = Totals::default();
    for index in order {
        let next = totals.with(bins[index], config);
        dispositions[index] = if next.bytes > config.max_job_input_bytes {
            BinDisposition::Deferred(DeferReason::JobInput)
        } else if next.files > config.max_job_files {
            BinDisposition::Deferred(DeferReason::JobFiles)
        } else if next.duration_millis(slots) > config.duration_budget_millis {
            BinDisposition::Deferred(DeferReason::Duration)
        } else {
            totals = next;
            BinDisposition::Admitted
        };
    }
    let estimate = Estimate {
        tasks: totals.tasks,
        files: totals.files,
        bytes: totals.bytes,
        files_removed: totals.files_removed,
        duration_millis: if totals.tasks == 0 {
            0
        } else {
            totals.duration_millis(slots)
        },
    };
    (dispositions, estimate)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn bin(files: u64, bytes: u64) -> BinSize {
        BinSize {
            files,
            bytes,
            has_deletes: false,
        }
    }

    fn config() -> PlannerConfig {
        PlannerConfig::default()
    }

    #[test]
    fn files_removed_counts_replacement_files_at_the_target_size() {
        let config = config();

        assert_eq!(files_removed(bin(10, 100 * MIB), &config), 9);
        assert_eq!(files_removed(bin(10, 300 * MIB), &config), 8);
        assert_eq!(files_removed(bin(1, 0), &config), 0);
    }

    #[test]
    fn the_benefit_threshold_needs_files_and_removals_or_deletes() {
        let config = config();
        let deleted = BinSize {
            has_deletes: true,
            ..bin(1, MIB)
        };

        let (dispositions, estimate) = admit(&[bin(4, MIB), bin(5, MIB), deleted], &config);

        assert_eq!(
            dispositions,
            [
                BinDisposition::NoBenefit,
                BinDisposition::Admitted,
                BinDisposition::Admitted
            ]
        );
        assert_eq!(
            (estimate.tasks, estimate.files, estimate.files_removed),
            (2, 6, 4)
        );
    }

    #[test]
    fn five_files_filling_four_targets_remove_too_few() {
        let config = config();

        let (dispositions, _) = admit(&[bin(5, 4 * 256 * MIB)], &config);

        assert_eq!(dispositions, [BinDisposition::NoBenefit]);
    }

    #[test]
    fn the_job_input_limit_defers_the_least_beneficial_bins() {
        let config = PlannerConfig {
            max_job_input_bytes: 100 * MIB,
            ..config()
        };

        let (dispositions, estimate) = admit(
            &[bin(5, 60 * MIB), bin(9, 60 * MIB), bin(5, 40 * MIB)],
            &config,
        );

        assert_eq!(
            dispositions,
            [
                BinDisposition::Deferred(DeferReason::JobInput),
                BinDisposition::Admitted,
                BinDisposition::Admitted
            ]
        );
        assert_eq!(estimate.bytes, 100 * MIB);
    }

    #[test]
    fn the_job_file_limit_defers_bins() {
        let config = PlannerConfig {
            max_job_files: 12,
            ..config()
        };

        let (dispositions, estimate) = admit(&[bin(8, MIB), bin(6, MIB), bin(5, MIB)], &config);

        assert_eq!(
            dispositions,
            [
                BinDisposition::Admitted,
                BinDisposition::Deferred(DeferReason::JobFiles),
                BinDisposition::Deferred(DeferReason::JobFiles)
            ]
        );
        assert_eq!(estimate.files, 8);
    }

    #[test]
    fn task_time_is_overhead_plus_bytes_over_throughput() {
        let config = config();

        assert_eq!(task_millis(0, &config), 2_000);
        assert_eq!(task_millis(64 * MIB, &config), 3_000);
        assert_eq!(task_millis(64 * MIB + 1, &config), 3_001);
    }

    #[test]
    fn the_duration_budget_bounds_every_dispatch_order() {
        // Each 640 MiB task takes 12 s. Three slots and a 30 s budget:
        // n tasks need ceil(12n / 3) + 8 seconds, so five fit (six need 32 s).
        let config = PlannerConfig {
            duration_budget_millis: 30_000,
            max_concurrent_groups: 3,
            ..config()
        };
        let bins = vec![bin(5, 640 * MIB); 7];

        let (dispositions, estimate) = admit(&bins, &config);

        let admitted = dispositions
            .iter()
            .filter(|disposition| **disposition == BinDisposition::Admitted)
            .count();
        assert_eq!(admitted, 5);
        assert_eq!(estimate.duration_millis, 28_000);
        assert!(
            dispositions[5..]
                .iter()
                .all(|disposition| *disposition == BinDisposition::Deferred(DeferReason::Duration))
        );
    }

    #[test]
    fn one_slot_runs_tasks_back_to_back() {
        let config = PlannerConfig {
            max_concurrent_groups: 1,
            ..config()
        };

        let (_, estimate) = admit(&[bin(5, 64 * MIB), bin(5, 64 * MIB)], &config);

        assert_eq!(estimate.duration_millis, 6_000);
    }

    #[test]
    fn an_empty_job_takes_no_time() {
        assert_eq!(admit(&[], &config()).1, Estimate::default());
    }
}
