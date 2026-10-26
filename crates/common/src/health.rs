//! The table health vocabulary and its thresholds (BS-028.T1, ADR 0059).
//!
//! The health model itself lives in `bergsmith-maintenance`, which owns the
//! statistics it reads. The finding kinds and the threshold set live here,
//! beside [`PolicySpec`](crate::policy::PolicySpec), because a maintenance
//! policy will reference them when health triggers land (BS-028.T3).

use std::fmt;

use serde::{Deserialize, Serialize};

const MIB: u64 = 1024 * 1024;

/// One measurable signal in the table health model, in report order.
///
/// The order is the order findings appear in an evaluation, so the primary
/// finding of a table is deterministic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// Live data files are smaller than a healthy table's files.
    SmallFiles,
    /// Delete files hold a large share of the table's bytes.
    DeletePressure,
    /// The table retains many snapshots.
    SnapshotBacklog,
    /// Data files are spread over many small manifests.
    ManifestFragmentation,
    /// One partition holds most of the table's data files.
    PartitionSkew,
    /// The newest statistics sample is old.
    StaleStatistics,
}

impl FindingKind {
    /// Every finding, in report order.
    pub const ALL: &'static [Self] = &[
        Self::SmallFiles,
        Self::DeletePressure,
        Self::SnapshotBacklog,
        Self::ManifestFragmentation,
        Self::PartitionSkew,
        Self::StaleStatistics,
    ];

    /// The stable name used in the API and in stored policy overrides.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SmallFiles => "small_files",
            Self::DeletePressure => "delete_pressure",
            Self::SnapshotBacklog => "snapshot_backlog",
            Self::ManifestFragmentation => "manifest_fragmentation",
            Self::PartitionSkew => "partition_skew",
            Self::StaleStatistics => "stale_statistics",
        }
    }

    /// Parses the stable name, or `None` if it is unrecognized.
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|kind| kind.as_str() == value)
    }

    /// The unit of the finding's measured value and thresholds.
    #[must_use]
    pub const fn unit(self) -> Unit {
        match self {
            Self::SmallFiles => Unit::Bytes,
            Self::DeletePressure | Self::PartitionSkew => Unit::PerMille,
            Self::SnapshotBacklog => Unit::Snapshots,
            Self::ManifestFragmentation => Unit::FilesPerManifest,
            Self::StaleStatistics => Unit::Seconds,
        }
    }

    /// Whether a larger measurement is worse or better than a smaller one.
    #[must_use]
    pub const fn direction(self) -> Direction {
        match self {
            Self::SmallFiles | Self::ManifestFragmentation => Direction::Below,
            Self::DeletePressure
            | Self::SnapshotBacklog
            | Self::PartitionSkew
            | Self::StaleStatistics => Direction::Above,
        }
    }
}

impl fmt::Display for FindingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The unit a finding's values are measured in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// Bytes.
    Bytes,
    /// Per-mille: one thousandth of a whole.
    PerMille,
    /// Retained snapshots.
    Snapshots,
    /// Live data files per manifest.
    FilesPerManifest,
    /// Seconds.
    Seconds,
}

impl Unit {
    /// The stable name used in the API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bytes => "bytes",
            Self::PerMille => "per_mille",
            Self::Snapshots => "snapshots",
            Self::FilesPerManifest => "files_per_manifest",
            Self::Seconds => "seconds",
        }
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which way a measurement moves from healthy toward worse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Values below the threshold are worse, so critical is the smaller bound.
    Below,
    /// Values above the threshold are worse, so critical is the larger bound.
    Above,
}

/// The warning and critical bounds for one finding, in the finding's unit.
///
/// An override sets both bounds together; a half-specified finding is
/// refused, because the two bounds are only meaningful as a ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Thresholds {
    /// The bound at which the finding is reported as a warning.
    pub warning: u64,
    /// The bound at which the finding is reported as critical.
    pub critical: u64,
}

/// The complete threshold set for the health model, with documented defaults.
///
/// Every field may be overridden per policy; an absent field keeps the
/// default. The defaults are the researched vendor heuristics where the plan
/// records one, and Bergsmith's own documented bounds elsewhere (ADR 0059).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HealthThresholds {
    /// Average live data-file size.
    pub small_files: Thresholds,
    /// Live delete bytes in per-mille of live data bytes.
    pub delete_pressure: Thresholds,
    /// Snapshots retained in the table's metadata.
    pub snapshot_backlog: Thresholds,
    /// Live data files per manifest.
    pub manifest_fragmentation: Thresholds,
    /// The busiest partition's share of live data files, in per-mille.
    pub partition_skew: Thresholds,
    /// Age of the newest statistics sample.
    pub stale_statistics: Thresholds,
}

impl HealthThresholds {
    /// The documented defaults.
    pub const DEFAULTS: Self = Self {
        small_files: Thresholds {
            warning: 128 * MIB,
            critical: 32 * MIB,
        },
        delete_pressure: Thresholds {
            warning: 100,
            critical: 500,
        },
        snapshot_backlog: Thresholds {
            warning: 1_000,
            critical: 5_000,
        },
        manifest_fragmentation: Thresholds {
            warning: 1_000,
            critical: 100,
        },
        partition_skew: Thresholds {
            warning: 500,
            critical: 800,
        },
        stale_statistics: Thresholds {
            warning: 3_600,
            critical: 21_600,
        },
    };

    /// The bounds for one finding.
    #[must_use]
    pub const fn threshold(&self, kind: FindingKind) -> Thresholds {
        match kind {
            FindingKind::SmallFiles => self.small_files,
            FindingKind::DeletePressure => self.delete_pressure,
            FindingKind::SnapshotBacklog => self.snapshot_backlog,
            FindingKind::ManifestFragmentation => self.manifest_fragmentation,
            FindingKind::PartitionSkew => self.partition_skew,
            FindingKind::StaleStatistics => self.stale_statistics,
        }
    }

    /// Checks every bound, so a stored override cannot invert the ladder.
    ///
    /// A bound must be positive, and the critical bound must be strictly more
    /// severe than the warning bound: smaller for a below-finding, larger for
    /// an above-finding.
    ///
    /// # Errors
    ///
    /// [`ThresholdError::Zero`] for a zero bound, or
    /// [`ThresholdError::Inverted`] when critical is not more severe.
    pub fn validate(&self) -> Result<(), ThresholdError> {
        for kind in FindingKind::ALL {
            let thresholds = self.threshold(*kind);
            if thresholds.warning == 0 {
                return Err(ThresholdError::Zero {
                    kind: *kind,
                    bound: "warning",
                });
            }
            if thresholds.critical == 0 {
                return Err(ThresholdError::Zero {
                    kind: *kind,
                    bound: "critical",
                });
            }
            let ordered = match kind.direction() {
                Direction::Below => thresholds.critical < thresholds.warning,
                Direction::Above => thresholds.critical > thresholds.warning,
            };
            if !ordered {
                return Err(ThresholdError::Inverted {
                    kind: *kind,
                    warning: thresholds.warning,
                    critical: thresholds.critical,
                });
            }
        }
        Ok(())
    }
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self::DEFAULTS
    }
}

/// Why a threshold set cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ThresholdError {
    /// A bound is zero, which no finding can cross meaningfully.
    #[error("health threshold for {kind} {bound} must be positive")]
    Zero {
        /// The finding.
        kind: FindingKind,
        /// `warning` or `critical`.
        bound: &'static str,
    },
    /// The critical bound is not more severe than the warning bound.
    #[error(
        "health thresholds for {kind} are inverted: warning {warning} is not more severe than critical {critical}"
    )]
    Inverted {
        /// The finding.
        kind: FindingKind,
        /// The warning bound as stored.
        warning: u64,
        /// The critical bound as stored.
        critical: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_names_round_trip() {
        for kind in FindingKind::ALL {
            assert_eq!(FindingKind::from_name(kind.as_str()), Some(*kind));
        }
        assert_eq!(FindingKind::from_name("unknown"), None);
    }

    #[test]
    fn finding_kinds_cross_the_wire_in_snake_case() {
        for kind in FindingKind::ALL {
            let json = serde_json::to_string(kind).expect("a finding kind serializes");
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
        }
        assert_eq!(
            serde_json::from_str::<FindingKind>("\"manifest_fragmentation\"")
                .expect("the stable name parses"),
            FindingKind::ManifestFragmentation
        );
    }

    #[test]
    fn every_finding_reports_its_documented_unit_and_direction() {
        let expected = [
            (FindingKind::SmallFiles, Unit::Bytes, Direction::Below),
            (
                FindingKind::DeletePressure,
                Unit::PerMille,
                Direction::Above,
            ),
            (
                FindingKind::SnapshotBacklog,
                Unit::Snapshots,
                Direction::Above,
            ),
            (
                FindingKind::ManifestFragmentation,
                Unit::FilesPerManifest,
                Direction::Below,
            ),
            (FindingKind::PartitionSkew, Unit::PerMille, Direction::Above),
            (
                FindingKind::StaleStatistics,
                Unit::Seconds,
                Direction::Above,
            ),
        ];
        assert_eq!(FindingKind::ALL.len(), expected.len());
        for (kind, unit, direction) in expected {
            assert_eq!(kind.unit(), unit, "{kind}");
            assert_eq!(kind.direction(), direction, "{kind}");
        }
    }

    #[test]
    fn defaults_are_the_documented_bounds() {
        let defaults = HealthThresholds::DEFAULTS;
        assert_eq!(defaults.small_files.warning, 128 * MIB);
        assert_eq!(defaults.small_files.critical, 32 * MIB);
        assert_eq!(defaults.delete_pressure.warning, 100);
        assert_eq!(defaults.delete_pressure.critical, 500);
        assert_eq!(defaults.snapshot_backlog.warning, 1_000);
        assert_eq!(defaults.snapshot_backlog.critical, 5_000);
        assert_eq!(defaults.manifest_fragmentation.warning, 1_000);
        assert_eq!(defaults.manifest_fragmentation.critical, 100);
        assert_eq!(defaults.partition_skew.warning, 500);
        assert_eq!(defaults.partition_skew.critical, 800);
        assert_eq!(defaults.stale_statistics.warning, 3_600);
        assert_eq!(defaults.stale_statistics.critical, 21_600);
        defaults.validate().expect("the defaults are valid");
    }

    #[test]
    fn zero_bounds_are_rejected() {
        let mut thresholds = HealthThresholds::DEFAULTS;
        thresholds.snapshot_backlog.warning = 0;
        assert_eq!(
            thresholds.validate(),
            Err(ThresholdError::Zero {
                kind: FindingKind::SnapshotBacklog,
                bound: "warning",
            })
        );

        let mut thresholds = HealthThresholds::DEFAULTS;
        thresholds.stale_statistics.critical = 0;
        assert_eq!(
            thresholds.validate(),
            Err(ThresholdError::Zero {
                kind: FindingKind::StaleStatistics,
                bound: "critical",
            })
        );
    }

    #[test]
    fn inverted_bounds_are_rejected_in_both_directions() {
        let mut thresholds = HealthThresholds::DEFAULTS;
        thresholds.small_files = Thresholds {
            warning: 32 * MIB,
            critical: 128 * MIB,
        };
        assert!(matches!(
            thresholds.validate(),
            Err(ThresholdError::Inverted {
                kind: FindingKind::SmallFiles,
                ..
            })
        ));

        let mut thresholds = HealthThresholds::DEFAULTS;
        thresholds.delete_pressure = Thresholds {
            warning: 500,
            critical: 100,
        };
        assert!(matches!(
            thresholds.validate(),
            Err(ThresholdError::Inverted {
                kind: FindingKind::DeletePressure,
                ..
            })
        ));
    }

    #[test]
    fn equal_bounds_are_rejected() {
        let mut thresholds = HealthThresholds::DEFAULTS;
        thresholds.partition_skew = Thresholds {
            warning: 500,
            critical: 500,
        };
        assert!(matches!(
            thresholds.validate(),
            Err(ThresholdError::Inverted { .. })
        ));
    }

    #[test]
    fn a_partial_override_keeps_the_other_defaults() {
        let thresholds: HealthThresholds =
            serde_json::from_str(r#"{"snapshot_backlog":{"warning":10,"critical":50}}"#)
                .expect("a partial override deserializes");
        assert_eq!(
            thresholds.snapshot_backlog,
            Thresholds {
                warning: 10,
                critical: 50,
            }
        );
        assert_eq!(
            thresholds.delete_pressure,
            HealthThresholds::DEFAULTS.delete_pressure
        );
        assert_eq!(
            thresholds.threshold(FindingKind::SnapshotBacklog).critical,
            50
        );
    }

    #[test]
    fn a_half_specified_finding_is_refused() {
        assert!(
            serde_json::from_str::<HealthThresholds>(r#"{"snapshot_backlog":{"warning":10}}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<HealthThresholds>(r#"{"unknown":{}}"#).is_err());
    }
}
