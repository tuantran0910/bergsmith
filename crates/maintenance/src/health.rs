//! Explainable table health (BS-028.T1, ADR 0059).
//!
//! [`evaluate`] is a pure function of the newest metadata-only sample, the
//! thresholds, and the current time. It reads no catalog and no storage, and
//! it never writes: the coordinator supplies the sample and its clock, so a
//! recorded evaluation is reproducible and tests fix time instead of sleeping.

use bergsmith_common::health::{FindingKind, HealthThresholds, ThresholdError, Thresholds, Unit};
use bergsmith_common::ids::PolicyId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::statistics::TableSample;

/// One thousandth of a whole, the scale of every per-mille threshold.
const PER_MILLE: u128 = 1_000;

/// How serious a finding is.
///
/// Ordered so the worst finding determines the table's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// The table is outside a healthy bound but still usable.
    Warning,
    /// The table needs attention soon.
    Critical,
}

impl Severity {
    /// The stable name used in the API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

/// The evaluated state of one table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    /// A sample exists and no finding fired.
    Healthy,
    /// The worst finding is a warning.
    Warning,
    /// The worst finding is critical.
    Critical,
}

impl HealthState {
    /// The stable name used in the API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Warning => "warning",
            Self::Critical => "critical",
        }
    }
}

impl From<Severity> for HealthState {
    fn from(severity: Severity) -> Self {
        match severity {
            Severity::Warning => Self::Warning,
            Severity::Critical => Self::Critical,
        }
    }
}

/// Where an evaluation's thresholds came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThresholdSource {
    /// The documented defaults in [`HealthThresholds::DEFAULTS`].
    Defaults,
    /// One policy's overrides, merged over the defaults.
    Policy(PolicyId),
}

/// One finding at warning or worse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Which signal fired.
    pub kind: FindingKind,
    /// How serious the crossing is.
    pub severity: Severity,
    /// The measured value, in [`Finding::unit`].
    pub measured: u64,
    /// The bound the measurement crossed, in the same unit.
    pub threshold: u64,
    /// A plain-language sentence composed from the parts above.
    pub reason: String,
}

impl Finding {
    /// The unit of [`Finding::measured`] and [`Finding::threshold`].
    #[must_use]
    pub const fn unit(&self) -> Unit {
        self.kind.unit()
    }
}

/// The health of one table as of one sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableHealth {
    /// The worst finding's severity, or healthy if none fired.
    pub state: HealthState,
    /// Findings in [`FindingKind::ALL`] order.
    pub findings: Vec<Finding>,
    /// The thresholds this evaluation used.
    pub source: ThresholdSource,
}

/// A maintenance action a finding recommends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecommendationAction {
    /// Rewrite small data files, or remove the deletes that pressure them.
    Compaction,
    /// Expire retained snapshots (BS-031).
    SnapshotExpiration,
    /// Rewrite manifests (BS-031).
    ManifestRewrite,
    /// Sample the table again; the collector does this on its interval.
    Resample,
}

impl RecommendationAction {
    /// The stable name used in the API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compaction => "compaction",
            Self::SnapshotExpiration => "snapshot_expiration",
            Self::ManifestRewrite => "manifest_rewrite",
            Self::Resample => "resample",
        }
    }
}

/// What a finding recommends, and why it cannot act yet if it cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recommendation {
    /// The finding this answers.
    pub kind: FindingKind,
    /// The operation that resolves it.
    pub action: RecommendationAction,
    /// Why the action is not runnable yet; `None` means a preview can run it.
    pub advisory: Option<&'static str>,
}

/// Maps one finding to the maintenance action that resolves it.
///
/// Only a whole-table compaction can run today. Partition-targeted
/// compaction, snapshot expiration, and manifest rewrite are advisory until
/// the planner or BS-031 provides them; a stale sample resolves itself on the
/// collector's interval.
#[must_use]
pub const fn recommend(kind: FindingKind) -> Recommendation {
    let (action, advisory) = match kind {
        FindingKind::SmallFiles | FindingKind::DeletePressure => {
            (RecommendationAction::Compaction, None)
        }
        FindingKind::PartitionSkew => (
            RecommendationAction::Compaction,
            Some("compaction cannot target one partition yet; a preview assesses the whole table"),
        ),
        FindingKind::SnapshotBacklog => (
            RecommendationAction::SnapshotExpiration,
            Some("snapshot expiration is not implemented yet (BS-031)"),
        ),
        FindingKind::ManifestFragmentation => (
            RecommendationAction::ManifestRewrite,
            Some("manifest rewrite is not implemented yet (BS-031)"),
        ),
        FindingKind::StaleStatistics => (
            RecommendationAction::Resample,
            Some("the collector samples the table on its interval"),
        ),
    };
    Recommendation {
        kind,
        action,
        advisory,
    }
}

/// Why an evaluation could not run.
#[derive(Debug, thiserror::Error)]
pub enum HealthError {
    /// The threshold set is unusable.
    #[error(transparent)]
    Thresholds(#[from] ThresholdError),
}

/// Evaluates one table's health.
///
/// The sample must be the newest stored sample for the table. `confirmed_at`
/// is when a sampling pass last read the table's metadata successfully, which
/// is not the sample's creation time: an unchanged table records no new sample
/// but is still confirmed fresh. `now` is the caller's clock; a confirmation
/// stamped in the future (clock skew across hosts) is treated as fresh rather
/// than making the evaluation fail.
///
/// # Errors
///
/// [`HealthError::Thresholds`] when the threshold set is invalid.
#[must_use = "the evaluation is the point; discarding it computes nothing useful"]
pub fn evaluate(
    sample: &TableSample,
    confirmed_at: DateTime<Utc>,
    thresholds: &HealthThresholds,
    source: ThresholdSource,
    now: DateTime<Utc>,
) -> Result<TableHealth, HealthError> {
    thresholds.validate()?;
    let mut findings = Vec::with_capacity(FindingKind::ALL.len());
    findings.extend(small_files(sample, thresholds.small_files));
    findings.extend(delete_pressure(sample, thresholds.delete_pressure));
    findings.extend(snapshot_backlog(
        sample.snapshot_count,
        thresholds.snapshot_backlog,
    ));
    findings.extend(manifest_fragmentation(
        sample,
        thresholds.manifest_fragmentation,
    ));
    findings.extend(partition_skew(sample, thresholds.partition_skew));
    findings.extend(stale_statistics(
        sample_age(confirmed_at, now),
        thresholds.stale_statistics,
    ));

    let state = findings
        .iter()
        .map(|finding| HealthState::from(finding.severity))
        .max()
        .unwrap_or(HealthState::Healthy);

    Ok(TableHealth {
        state,
        findings,
        source,
    })
}

/// The average live data-file size, when the table has a distribution.
fn small_files(sample: &TableSample, thresholds: Thresholds) -> Option<Finding> {
    if sample.data_files < 2 {
        return None;
    }
    let measured = sample.data_bytes / sample.data_files;
    let (severity, threshold) = below(measured, thresholds)?;
    Some(Finding {
        kind: FindingKind::SmallFiles,
        severity,
        measured,
        threshold,
        reason: format!(
            "average data file size is {measured} bytes across {} files",
            sample.data_files
        ),
    })
}

/// Delete bytes against data bytes, when both exist.
fn delete_pressure(sample: &TableSample, thresholds: Thresholds) -> Option<Finding> {
    if sample.delete_bytes == 0 || sample.data_bytes == 0 {
        return None;
    }
    let (severity, threshold) =
        strictly_above_ratio(sample.delete_bytes, sample.data_bytes, thresholds)?;
    let measured = per_mille(sample.delete_bytes, sample.data_bytes);
    Some(Finding {
        kind: FindingKind::DeletePressure,
        severity,
        measured,
        threshold,
        reason: format!(
            "delete files are {measured} per-mille of data bytes across {} delete files",
            sample.delete_files
        ),
    })
}

/// Retained snapshots.
fn snapshot_backlog(snapshots: u64, thresholds: Thresholds) -> Option<Finding> {
    let (severity, threshold) = at_or_above(snapshots, thresholds)?;
    Some(Finding {
        kind: FindingKind::SnapshotBacklog,
        severity,
        measured: snapshots,
        threshold,
        reason: format!("the table retains {snapshots} snapshots"),
    })
}

/// Live data files per data manifest, for tables large enough to have a
/// layout. Delete manifests are excluded: data files cannot be spread over
/// them, and counting them would overstate fragmentation.
fn manifest_fragmentation(sample: &TableSample, thresholds: Thresholds) -> Option<Finding> {
    if sample.data_manifest_count < 2 || sample.data_files < 100 {
        return None;
    }
    let measured = sample.data_files / sample.data_manifest_count;
    let (severity, threshold) = below(measured, thresholds)?;
    Some(Finding {
        kind: FindingKind::ManifestFragmentation,
        severity,
        measured,
        threshold,
        reason: format!(
            "{} data files are spread over {} data manifests",
            sample.data_files, sample.data_manifest_count
        ),
    })
}

/// The busiest partition's share of data files, for partitioned tables.
fn partition_skew(sample: &TableSample, thresholds: Thresholds) -> Option<Finding> {
    if sample.partition_count < 2 || sample.data_files < 100 {
        return None;
    }
    // The sample's count survives a zero display bound, and older samples
    // recorded before the count existed fall back to the bounded list.
    let busiest = sample.max_partition_files.max(
        sample
            .top_partitions
            .iter()
            .map(|partition| partition.files)
            .max()
            .unwrap_or(0),
    );
    let (severity, threshold) = at_or_above_ratio(busiest, sample.data_files, thresholds)?;
    let measured = per_mille(busiest, sample.data_files);
    Some(Finding {
        kind: FindingKind::PartitionSkew,
        severity,
        measured,
        threshold,
        reason: format!(
            "the busiest partition holds {measured} per-mille of {} data files",
            sample.data_files
        ),
    })
}

/// The age of the last successful confirmation; a future stamp counts fresh.
fn stale_statistics(age: u64, thresholds: Thresholds) -> Option<Finding> {
    let (severity, threshold) = at_or_above(age, thresholds)?;
    Some(Finding {
        kind: FindingKind::StaleStatistics,
        severity,
        measured: age,
        threshold,
        reason: format!("the table metadata was last confirmed {age} seconds ago"),
    })
}

/// Whole seconds between the confirmation and now, clamped at zero for skew.
fn sample_age(confirmed_at: DateTime<Utc>, now: DateTime<Utc>) -> u64 {
    let seconds = now.signed_duration_since(confirmed_at).num_seconds().max(0);
    u64::try_from(seconds).unwrap_or(u64::MAX)
}

/// A below-finding: a smaller measurement is worse, so critical is lower.
fn below(measured: u64, thresholds: Thresholds) -> Option<(Severity, u64)> {
    if measured < thresholds.critical {
        Some((Severity::Critical, thresholds.critical))
    } else if measured < thresholds.warning {
        Some((Severity::Warning, thresholds.warning))
    } else {
        None
    }
}

/// An above-finding whose bound the measurement reaches: equality fires.
fn at_or_above(measured: u64, thresholds: Thresholds) -> Option<(Severity, u64)> {
    if measured >= thresholds.critical {
        Some((Severity::Critical, thresholds.critical))
    } else if measured >= thresholds.warning {
        Some((Severity::Warning, thresholds.warning))
    } else {
        None
    }
}

/// An above-finding expressed as a ratio that must be strictly exceeded.
fn strictly_above_ratio(
    numerator: u64,
    denominator: u64,
    thresholds: Thresholds,
) -> Option<(Severity, u64)> {
    let exceeds = |bound: u64| {
        u128::from(numerator) * PER_MILLE > u128::from(bound) * u128::from(denominator)
    };
    if exceeds(thresholds.critical) {
        Some((Severity::Critical, thresholds.critical))
    } else if exceeds(thresholds.warning) {
        Some((Severity::Warning, thresholds.warning))
    } else {
        None
    }
}

/// An above-finding expressed as a ratio that reaches its bound.
fn at_or_above_ratio(
    numerator: u64,
    denominator: u64,
    thresholds: Thresholds,
) -> Option<(Severity, u64)> {
    let reaches = |bound: u64| {
        u128::from(numerator) * PER_MILLE >= u128::from(bound) * u128::from(denominator)
    };
    if reaches(thresholds.critical) {
        Some((Severity::Critical, thresholds.critical))
    } else if reaches(thresholds.warning) {
        Some((Severity::Warning, thresholds.warning))
    } else {
        None
    }
}

/// Converts a ratio to per-mille, rounding down; used for the reported value.
fn per_mille(numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 {
        return 0;
    }
    u64::try_from(u128::from(numerator) * PER_MILLE / u128::from(denominator)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use bergsmith_common::health::FindingKind;
    use chrono::TimeDelta;

    use super::*;
    use crate::statistics::PartitionFiles;

    const MIB: u64 = 1024 * 1024;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).expect("a fixed instant")
    }

    fn sample() -> TableSample {
        TableSample {
            metadata_location: "s3://bucket/table/metadata/v1.metadata.json".to_owned(),
            snapshot_id: Some(1),
            last_operation: Some("append".to_owned()),
            record_count: 0,
            data_files: 0,
            data_bytes: 0,
            delete_files: 0,
            delete_bytes: 0,
            position_delete_files: 0,
            equality_delete_files: 0,
            position_delete_records: 0,
            equality_delete_records: 0,
            manifest_count: 0,
            data_manifest_count: 0,
            manifest_bytes: 0,
            partition_count: 0,
            small_files: 0,
            target_file_size_bytes: 512 * MIB,
            size_histogram: Vec::new(),
            top_partitions: Vec::new(),
            max_partition_files: 0,
            snapshot_count: 0,
            oldest_snapshot_at: None,
            newest_snapshot_at: None,
        }
    }

    fn partition(files: u64) -> PartitionFiles {
        PartitionFiles {
            spec_id: 0,
            partition: "day=2026-10-20".to_owned(),
            files,
            bytes: 0,
            delete_files: 0,
        }
    }

    fn health(sample: &TableSample) -> TableHealth {
        evaluated_at(sample, now())
    }

    fn evaluated_at(sample: &TableSample, confirmed_at: DateTime<Utc>) -> TableHealth {
        evaluate(
            sample,
            confirmed_at,
            &HealthThresholds::default(),
            ThresholdSource::Defaults,
            now(),
        )
        .expect("the default thresholds are valid")
    }

    fn kind(health: &TableHealth, kind: FindingKind) -> Option<Finding> {
        health
            .findings
            .iter()
            .find(|finding| finding.kind == kind)
            .cloned()
    }

    #[test]
    fn a_fresh_table_without_pressure_is_healthy() {
        let mut sample = sample();
        sample.data_files = 4;
        sample.data_bytes = 4 * 512 * MIB;
        sample.snapshot_count = 10;
        sample.manifest_count = 2;
        sample.partition_count = 2;
        sample.top_partitions = vec![partition(2), partition(2)];
        let health = health(&sample);
        assert_eq!(health.state, HealthState::Healthy);
        assert!(health.findings.is_empty());
        assert_eq!(health.source, ThresholdSource::Defaults);
    }

    #[test]
    fn small_files_crosses_at_the_documented_boundaries() {
        let mut sample = sample();
        sample.data_files = 2;

        sample.data_bytes = 2 * 128 * MIB;
        assert!(kind(&health(&sample), FindingKind::SmallFiles).is_none());

        sample.data_bytes = 2 * (128 * MIB - 1);
        let found = kind(&health(&sample), FindingKind::SmallFiles)
            .expect("one byte below the warning bound warns");
        assert_eq!(found.severity, Severity::Warning);
        assert_eq!(found.measured, 128 * MIB - 1);
        assert_eq!(found.threshold, 128 * MIB);
        assert_eq!(found.unit(), Unit::Bytes);

        sample.data_bytes = 2 * 32 * MIB;
        let found = kind(&health(&sample), FindingKind::SmallFiles)
            .expect("the critical bound is still a warning");
        assert_eq!(found.severity, Severity::Warning);

        sample.data_bytes = 2 * (32 * MIB - 1);
        let found = kind(&health(&sample), FindingKind::SmallFiles)
            .expect("one byte below the critical bound is critical");
        assert_eq!(found.severity, Severity::Critical);
        assert_eq!(found.measured, 32 * MIB - 1);
        assert_eq!(found.threshold, 32 * MIB);
    }

    #[test]
    fn a_single_file_is_not_judged_for_small_files() {
        let mut sample = sample();
        sample.data_files = 1;
        sample.data_bytes = 1;
        assert!(kind(&health(&sample), FindingKind::SmallFiles).is_none());
    }

    #[test]
    fn delete_pressure_must_exceed_its_bounds() {
        let mut sample = sample();
        sample.data_files = 1;
        sample.data_bytes = 1_000_000;

        sample.delete_files = 0;
        sample.delete_bytes = 0;
        assert!(kind(&health(&sample), FindingKind::DeletePressure).is_none());

        sample.delete_files = 1;
        sample.delete_bytes = 100_000;
        assert!(
            kind(&health(&sample), FindingKind::DeletePressure).is_none(),
            "exactly one tenth is not above the warning bound"
        );

        sample.delete_bytes = 100_001;
        let found = kind(&health(&sample), FindingKind::DeletePressure)
            .expect("just above one tenth warns");
        assert_eq!(found.severity, Severity::Warning);
        assert_eq!(found.threshold, 100);
        assert_eq!(found.measured, 100);
        assert_eq!(found.unit(), Unit::PerMille);

        sample.delete_bytes = 500_000;
        let found = kind(&health(&sample), FindingKind::DeletePressure)
            .expect("exactly half is still a warning");
        assert_eq!(found.severity, Severity::Warning);

        sample.delete_bytes = 500_001;
        let found = kind(&health(&sample), FindingKind::DeletePressure)
            .expect("just above half is critical");
        assert_eq!(found.severity, Severity::Critical);
        assert_eq!(found.threshold, 500);
    }

    #[test]
    fn deletes_without_data_bytes_are_not_judged() {
        let mut sample = sample();
        sample.delete_files = 3;
        sample.delete_bytes = 10 * MIB;
        sample.data_bytes = 0;
        assert!(kind(&health(&sample), FindingKind::DeletePressure).is_none());
    }

    #[test]
    fn snapshot_backlog_fires_at_its_bounds() {
        let mut sample = sample();
        sample.data_files = 2;
        sample.data_bytes = 2 * 512 * MIB;

        sample.snapshot_count = 999;
        assert!(kind(&health(&sample), FindingKind::SnapshotBacklog).is_none());

        sample.snapshot_count = 1_000;
        let found = kind(&health(&sample), FindingKind::SnapshotBacklog)
            .expect("the warning bound fires at equality");
        assert_eq!(found.severity, Severity::Warning);
        assert_eq!(found.measured, 1_000);
        assert_eq!(found.threshold, 1_000);
        assert_eq!(found.unit(), Unit::Snapshots);

        sample.snapshot_count = 4_999;
        assert_eq!(
            kind(&health(&sample), FindingKind::SnapshotBacklog).map(|finding| finding.severity),
            Some(Severity::Warning)
        );

        sample.snapshot_count = 5_000;
        assert_eq!(
            kind(&health(&sample), FindingKind::SnapshotBacklog).map(|finding| finding.severity),
            Some(Severity::Critical)
        );
    }

    #[test]
    fn manifest_fragmentation_is_guarded_and_crosses_its_bounds() {
        let mut sample = sample();
        sample.data_files = 10_000;
        sample.manifest_count = 1;
        sample.data_manifest_count = 1;
        assert!(
            kind(&health(&sample), FindingKind::ManifestFragmentation).is_none(),
            "one data manifest is a normal seeding shape"
        );

        sample.data_files = 99;
        sample.manifest_count = 2;
        sample.data_manifest_count = 2;
        assert!(
            kind(&health(&sample), FindingKind::ManifestFragmentation).is_none(),
            "a small table is not judged for manifest layout"
        );

        sample.data_files = 2_000;
        sample.data_manifest_count = 2;
        assert!(kind(&health(&sample), FindingKind::ManifestFragmentation).is_none());

        sample.data_files = 1_999;
        let found = kind(&health(&sample), FindingKind::ManifestFragmentation)
            .expect("999 files per manifest warns");
        assert_eq!(found.severity, Severity::Warning);
        assert_eq!(found.measured, 999);
        assert_eq!(found.threshold, 1_000);
        assert_eq!(found.unit(), Unit::FilesPerManifest);

        sample.data_files = 200;
        let found = kind(&health(&sample), FindingKind::ManifestFragmentation)
            .expect("100 files per manifest warns");
        assert_eq!(found.severity, Severity::Warning);

        sample.data_files = 199;
        let found = kind(&health(&sample), FindingKind::ManifestFragmentation)
            .expect("99 files per manifest is critical");
        assert_eq!(found.severity, Severity::Critical);
        assert_eq!(found.measured, 99);
        assert_eq!(found.threshold, 100);
    }

    #[test]
    fn delete_manifests_do_not_dilute_fragmentation() {
        let mut sample = sample();
        sample.data_files = 100;
        sample.manifest_count = 2;
        sample.data_manifest_count = 1;
        assert!(
            kind(&health(&sample), FindingKind::ManifestFragmentation).is_none(),
            "one data manifest plus one delete manifest is not fragmentation"
        );

        sample.manifest_count = 3;
        sample.data_manifest_count = 2;
        let found = kind(&health(&sample), FindingKind::ManifestFragmentation)
            .expect("two data manifests hold 50 files each");
        assert_eq!(found.severity, Severity::Critical);
        assert_eq!(found.measured, 50);
        assert_eq!(found.threshold, 100);
    }

    #[test]
    fn partition_skew_fires_at_its_share_bounds() {
        let mut sample = sample();
        sample.partition_count = 2;
        sample.data_files = 100;
        sample.top_partitions = vec![partition(49), partition(20)];
        assert!(kind(&health(&sample), FindingKind::PartitionSkew).is_none());

        sample.top_partitions = vec![partition(50), partition(50)];
        let found = kind(&health(&sample), FindingKind::PartitionSkew)
            .expect("half the files in one partition warns");
        assert_eq!(found.severity, Severity::Warning);
        assert_eq!(found.measured, 500);
        assert_eq!(found.threshold, 500);
        assert_eq!(found.unit(), Unit::PerMille);

        sample.top_partitions = vec![partition(79), partition(21)];
        assert_eq!(
            kind(&health(&sample), FindingKind::PartitionSkew).map(|finding| finding.severity),
            Some(Severity::Warning)
        );

        sample.top_partitions = vec![partition(80), partition(20)];
        assert_eq!(
            kind(&health(&sample), FindingKind::PartitionSkew).map(|finding| finding.severity),
            Some(Severity::Critical)
        );
    }

    #[test]
    fn skew_survives_an_empty_display_list() {
        let mut sample = sample();
        sample.partition_count = 2;
        sample.data_files = 100;
        sample.top_partitions = Vec::new();
        sample.max_partition_files = 80;
        let found = kind(&health(&sample), FindingKind::PartitionSkew)
            .expect("the stored busiest count does not depend on the display bound");
        assert_eq!(found.severity, Severity::Critical);
        assert_eq!(found.measured, 800);
    }

    #[test]
    fn skew_uses_the_larger_of_the_stored_count_and_the_list() {
        let mut sample = sample();
        sample.partition_count = 2;
        sample.data_files = 100;
        sample.top_partitions = vec![partition(20)];
        sample.max_partition_files = 90;
        assert_eq!(
            kind(&health(&sample), FindingKind::PartitionSkew).map(|finding| finding.severity),
            Some(Severity::Critical)
        );
    }

    #[test]
    fn partition_skew_is_guarded_for_small_or_unpartitioned_tables() {
        let mut sample = sample();
        sample.partition_count = 1;
        sample.data_files = 1_000;
        sample.top_partitions = vec![partition(1_000)];
        assert!(kind(&health(&sample), FindingKind::PartitionSkew).is_none());

        sample.partition_count = 2;
        sample.data_files = 99;
        sample.top_partitions = vec![partition(98), partition(1)];
        assert!(kind(&health(&sample), FindingKind::PartitionSkew).is_none());
    }

    #[test]
    fn stale_statistics_crosses_at_the_documented_boundaries() {
        let sample = sample();

        let at = |seconds: i64| now() - TimeDelta::seconds(seconds);
        let evaluate_at = |seconds: i64| {
            evaluate(
                &sample,
                at(seconds),
                &HealthThresholds::default(),
                ThresholdSource::Defaults,
                now(),
            )
            .expect("valid thresholds")
        };

        assert!(kind(&evaluate_at(3_599), FindingKind::StaleStatistics).is_none());
        let found = evaluate_at(3_600);
        let found = kind(&found, FindingKind::StaleStatistics).expect("one hour warns");
        assert_eq!(found.severity, Severity::Warning);
        assert_eq!(found.measured, 3_600);
        assert_eq!(found.threshold, 3_600);
        assert_eq!(found.unit(), Unit::Seconds);

        assert_eq!(
            kind(&evaluate_at(21_599), FindingKind::StaleStatistics)
                .map(|finding| finding.severity),
            Some(Severity::Warning)
        );
        assert_eq!(
            kind(&evaluate_at(21_600), FindingKind::StaleStatistics)
                .map(|finding| finding.severity),
            Some(Severity::Critical)
        );
    }

    #[test]
    fn staleness_measures_the_last_confirmation_not_the_sample() {
        // The sample is ten hours old, but a pass confirmed the metadata ten
        // minutes ago: an unchanged table is fresh.
        let sample = sample();
        let health = evaluated_at(&sample, now() - TimeDelta::minutes(10));
        assert!(kind(&health, FindingKind::StaleStatistics).is_none());

        // No pass has succeeded for seven hours, so the table is stale even
        // though a sample may have been written later by the publication path.
        let health = evaluated_at(&sample, now() - TimeDelta::hours(7));
        let found = kind(&health, FindingKind::StaleStatistics).expect("seven hours is stale");
        assert_eq!(found.severity, Severity::Critical);
        assert_eq!(found.measured, 7 * 3_600);
    }

    #[test]
    fn a_confirmation_stamped_in_the_future_is_treated_as_fresh() {
        let sample = sample();
        let health = evaluated_at(&sample, now() + TimeDelta::hours(1));
        assert!(kind(&health, FindingKind::StaleStatistics).is_none());
    }

    #[test]
    fn the_worst_finding_sets_the_state_and_findings_keep_kind_order() {
        let mut sample = sample();
        sample.data_files = 2;
        sample.data_bytes = 2 * 8 * MIB;
        sample.snapshot_count = 1_000;
        let health = health(&sample);
        assert_eq!(health.state, HealthState::Critical);
        let kinds: Vec<FindingKind> = health.findings.iter().map(|finding| finding.kind).collect();
        assert_eq!(
            kinds,
            vec![FindingKind::SmallFiles, FindingKind::SnapshotBacklog]
        );
        assert_eq!(health.findings[0].severity, Severity::Critical);
        assert_eq!(health.findings[1].severity, Severity::Warning);
    }

    #[test]
    fn every_finding_maps_to_the_action_that_resolves_it() {
        let expected = [
            (
                FindingKind::SmallFiles,
                RecommendationAction::Compaction,
                false,
            ),
            (
                FindingKind::DeletePressure,
                RecommendationAction::Compaction,
                false,
            ),
            (
                FindingKind::PartitionSkew,
                RecommendationAction::Compaction,
                true,
            ),
            (
                FindingKind::SnapshotBacklog,
                RecommendationAction::SnapshotExpiration,
                true,
            ),
            (
                FindingKind::ManifestFragmentation,
                RecommendationAction::ManifestRewrite,
                true,
            ),
            (
                FindingKind::StaleStatistics,
                RecommendationAction::Resample,
                true,
            ),
        ];
        assert_eq!(FindingKind::ALL.len(), expected.len());
        for (kind, action, advisory) in expected {
            let recommendation = recommend(kind);
            assert_eq!(recommendation.kind, kind);
            assert_eq!(recommendation.action, action, "{kind}");
            assert_eq!(recommendation.advisory.is_some(), advisory, "{kind}");
            if let Some(reason) = recommendation.advisory {
                assert!(!reason.is_empty(), "{kind}");
            }
        }
    }

    #[test]
    fn evaluation_is_deterministic() {
        let mut sample = sample();
        sample.data_files = 4;
        sample.data_bytes = 4 * 40 * MIB;
        sample.snapshot_count = 2_000;
        assert_eq!(health(&sample), health(&sample));
    }

    #[test]
    fn a_policy_source_is_carried_through() {
        let policy = PolicyId::generate();
        let health = evaluate(
            &sample(),
            now(),
            &HealthThresholds::default(),
            ThresholdSource::Policy(policy),
            now(),
        )
        .expect("valid thresholds");
        assert_eq!(health.source, ThresholdSource::Policy(policy));
    }

    #[test]
    fn invalid_thresholds_are_refused() {
        let mut thresholds = HealthThresholds::default();
        thresholds.snapshot_backlog.warning = 5_000;
        thresholds.snapshot_backlog.critical = 1_000;
        assert!(matches!(
            evaluate(
                &sample(),
                now(),
                &thresholds,
                ThresholdSource::Defaults,
                now()
            ),
            Err(HealthError::Thresholds(_))
        ));
    }
}
