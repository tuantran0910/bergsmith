//! Metadata-only table statistics (BS-025.T1).
//!
//! A sample is read from the table's current metadata, its manifest list, and
//! its manifests. No data file and no delete file is opened, so sampling is
//! bounded by [`StatisticsConfig::max_manifest_entries`] rather than by table
//! size.
//!
//! Counters come from the live manifest entries rather than from the
//! snapshot summary: entries are authoritative, they carry the delete kind, and
//! the walk is required for the histogram anyway. The summary contributes only
//! the last commit's operation.
//!
//! Everything is ordered: partitions sort by key, the histogram has a fixed
//! bucket list, and the top partitions break ties by label, so two samples of
//! the same state are byte-identical after serialization.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use iceberg::io::FileIO;
use iceberg::spec::{
    DataContentType, Datum, FormatVersion, ManifestList, PrimitiveLiteral, TableMetadata,
};
use serde::{Deserialize, Serialize};

use crate::error::PlanError;
use crate::partition::{PartitionKey, Specs};

/// The table property that names the target data-file size.
const TARGET_FILE_SIZE_PROPERTY: &str = "write.target-file-size-bytes";

const MIB: u64 = 1024 * 1024;

/// Bounds for one metadata-only sample.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatisticsConfig {
    /// The most live manifest entries one sample reads. A table above the
    /// bound is skipped until an operator raises it, exactly as planning
    /// refuses it.
    pub max_manifest_entries: u64,
    /// Ascending, exclusive upper bounds of the size histogram, in bytes. A
    /// sample always has one bucket more than this list: the final bucket is
    /// open-ended.
    pub size_buckets_bytes: Vec<u64>,
    /// The target file size used when the table does not set
    /// `write.target-file-size-bytes`.
    pub default_target_file_size_bytes: u64,
    /// How many partitions a sample keeps, ordered by file count. The rest
    /// still count toward [`TableSample::partition_count`].
    pub top_partitions: usize,
}

impl Default for StatisticsConfig {
    fn default() -> Self {
        Self {
            max_manifest_entries: 200_000,
            size_buckets_bytes: vec![MIB, 8 * MIB, 32 * MIB, 128 * MIB, 512 * MIB],
            default_target_file_size_bytes: 512 * MIB,
            top_partitions: 10,
        }
    }
}

impl StatisticsConfig {
    /// Checks the bounds a sample relies on.
    ///
    /// # Errors
    ///
    /// [`StatisticsError::InvalidConfig`] when the entry bound or the target
    /// size is zero, or the bucket bounds are empty or not strictly ascending.
    pub fn validate(&self) -> Result<(), StatisticsError> {
        if self.max_manifest_entries == 0 {
            return Err(StatisticsError::InvalidConfig {
                reason: "max_manifest_entries must be positive",
            });
        }
        if self.default_target_file_size_bytes == 0 {
            return Err(StatisticsError::InvalidConfig {
                reason: "default_target_file_size_bytes must be positive",
            });
        }
        if self.size_buckets_bytes.is_empty() {
            return Err(StatisticsError::InvalidConfig {
                reason: "size_buckets_bytes must have at least one bound",
            });
        }
        if self
            .size_buckets_bytes
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        {
            return Err(StatisticsError::InvalidConfig {
                reason: "size_buckets_bytes must be strictly ascending",
            });
        }
        Ok(())
    }
}

/// Why a table could not be sampled.
#[derive(Debug, thiserror::Error)]
pub enum StatisticsError {
    /// The table has more live manifest entries than one sample may read.
    #[error("table has more than {limit} live manifest entries")]
    TooLarge {
        /// The configured bound.
        limit: u64,
    },
    /// The configured bounds are unusable.
    #[error("statistics configuration is invalid: {reason}")]
    InvalidConfig {
        /// What is wrong.
        reason: &'static str,
    },
    /// A partition spec could not be bound to a schema.
    #[error(transparent)]
    Partition(#[from] PlanError),
    /// Reading the manifest list or a manifest failed.
    #[error("reading table metadata failed: {0}")]
    Storage(#[from] iceberg::Error),
}

/// One histogram bucket of data-file sizes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizeBucket {
    /// The inclusive lower bound, in bytes.
    pub lower_bound_bytes: u64,
    /// The exclusive upper bound, in bytes; `None` for the final bucket.
    pub upper_bound_bytes: Option<u64>,
    /// Data files in the bucket.
    pub files: u64,
    /// Bytes of those data files.
    pub bytes: u64,
}

/// One partition of the current snapshot, as a readable label and its file
/// count. Values use the spec's field names and the Iceberg single-value
/// encoding decoded back to text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartitionFiles {
    /// The partition spec ID.
    pub spec_id: i32,
    /// `field=value` pairs in partition-field order; empty when unpartitioned.
    pub partition: String,
    /// Data files in the partition.
    pub files: u64,
    /// Bytes of those data files.
    pub bytes: u64,
}

/// Everything one metadata-only pass records about a table's current state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableSample {
    /// The `metadata.json` the sample describes. An unchanged location means
    /// the counters are unchanged too.
    pub metadata_location: String,
    /// The current snapshot, or `None` for a table with no snapshot.
    pub snapshot_id: Option<i64>,
    /// The current snapshot's operation, such as `append` or `replace`.
    pub last_operation: Option<String>,
    /// Rows in live data files, as their manifest entries record them.
    pub record_count: u64,
    /// Live data files.
    pub data_files: u64,
    /// Bytes of live data files.
    pub data_bytes: u64,
    /// Live position and equality delete files.
    pub delete_files: u64,
    /// Bytes of live delete files.
    pub delete_bytes: u64,
    /// Live position delete files.
    pub position_delete_files: u64,
    /// Live equality delete files.
    pub equality_delete_files: u64,
    /// Rows recorded by live position delete files.
    pub position_delete_records: u64,
    /// Rows recorded by live equality delete files.
    pub equality_delete_records: u64,
    /// Manifests in the current snapshot's manifest list.
    pub manifest_count: u64,
    /// Bytes of those manifests.
    pub manifest_bytes: u64,
    /// Distinct partitions among live data files, across every spec.
    pub partition_count: u64,
    /// Data files smaller than [`Self::target_file_size_bytes`].
    pub small_files: u64,
    /// The target size small files are measured against.
    pub target_file_size_bytes: u64,
    /// Data-file sizes, one entry per configured bucket.
    pub size_histogram: Vec<SizeBucket>,
    /// The busiest partitions, bounded by
    /// [`StatisticsConfig::top_partitions`].
    pub top_partitions: Vec<PartitionFiles>,
    /// Snapshots the table retains.
    pub snapshot_count: u64,
    /// The oldest entry in the snapshot log.
    pub oldest_snapshot_at: Option<DateTime<Utc>>,
    /// The newest entry in the snapshot log: the last commit.
    pub newest_snapshot_at: Option<DateTime<Utc>>,
}

impl TableSample {
    /// Reads `metadata` from `metadata_location` through `file_io`.
    ///
    /// A table with no current snapshot produces an empty sample rather than
    /// an error: there is no file to count yet.
    ///
    /// # Errors
    ///
    /// - [`StatisticsError::InvalidConfig`] when `config` is unusable.
    /// - [`StatisticsError::TooLarge`] when the table exceeds
    ///   [`StatisticsConfig::max_manifest_entries`] live entries.
    /// - [`StatisticsError::Partition`] when a spec cannot be bound.
    /// - [`StatisticsError::Storage`] when the manifest list or a manifest
    ///   cannot be read.
    pub async fn collect(
        metadata: &TableMetadata,
        metadata_location: &str,
        file_io: &FileIO,
        config: &StatisticsConfig,
    ) -> Result<Self, StatisticsError> {
        config.validate()?;
        let specs = Specs::of(metadata)?;
        let target_file_size_bytes = target_file_size(metadata, config);
        let (oldest_snapshot_at, newest_snapshot_at) = snapshot_bounds(metadata);
        let mut sample = Self {
            metadata_location: metadata_location.to_owned(),
            snapshot_id: None,
            last_operation: None,
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
            manifest_bytes: 0,
            partition_count: 0,
            small_files: 0,
            target_file_size_bytes,
            size_histogram: empty_histogram(&config.size_buckets_bytes),
            top_partitions: Vec::new(),
            snapshot_count: u64::try_from(metadata.snapshots().len()).unwrap_or(u64::MAX),
            oldest_snapshot_at,
            newest_snapshot_at,
        };
        let Some(snapshot) = metadata.current_snapshot() else {
            return Ok(sample);
        };
        sample.snapshot_id = Some(snapshot.snapshot_id());
        sample.last_operation = Some(snapshot.summary().operation.as_str().to_owned());
        sample
            .walk(&specs, snapshot.manifest_list(), file_io, config)
            .await?;
        Ok(sample)
    }

    /// Counts the live files of one snapshot's manifest list into this sample.
    async fn walk(
        &mut self,
        specs: &Specs,
        manifest_list: &str,
        file_io: &FileIO,
        config: &StatisticsConfig,
    ) -> Result<(), StatisticsError> {
        let list = file_io.new_input(manifest_list)?.read().await?;
        let list = ManifestList::parse_with_version(&list, FormatVersion::V2)?;
        let mut partitions: BTreeMap<PartitionKey, (u64, u64)> = BTreeMap::new();
        let mut live: u64 = 0;
        for manifest_file in list.entries() {
            self.manifest_count += 1;
            self.manifest_bytes = self
                .manifest_bytes
                .saturating_add(u64::try_from(manifest_file.manifest_length).unwrap_or(0));
            let manifest = manifest_file.load_manifest(file_io).await?;
            for entry in manifest.entries().iter().filter(|entry| entry.is_alive()) {
                live += 1;
                if live > config.max_manifest_entries {
                    return Err(StatisticsError::TooLarge {
                        limit: config.max_manifest_entries,
                    });
                }
                let file = entry.data_file();
                let size = file.file_size_in_bytes();
                let records = file.record_count();
                match file.content_type() {
                    DataContentType::Data => {
                        self.data_files += 1;
                        self.data_bytes = self.data_bytes.saturating_add(size);
                        self.record_count = self.record_count.saturating_add(records);
                        if size < self.target_file_size_bytes {
                            self.small_files += 1;
                        }
                        add_to_bucket(&mut self.size_histogram, &config.size_buckets_bytes, size);
                        let key = specs.key(file.partition_spec_id(), file.partition())?;
                        let partition = partitions.entry(key).or_insert((0, 0));
                        partition.0 += 1;
                        partition.1 = partition.1.saturating_add(size);
                    }
                    DataContentType::PositionDeletes => {
                        self.delete_files += 1;
                        self.delete_bytes = self.delete_bytes.saturating_add(size);
                        self.position_delete_files += 1;
                        self.position_delete_records =
                            self.position_delete_records.saturating_add(records);
                    }
                    DataContentType::EqualityDeletes => {
                        self.delete_files += 1;
                        self.delete_bytes = self.delete_bytes.saturating_add(size);
                        self.equality_delete_files += 1;
                        self.equality_delete_records =
                            self.equality_delete_records.saturating_add(records);
                    }
                }
            }
        }

        self.partition_count = u64::try_from(partitions.len()).unwrap_or(u64::MAX);
        let mut top: Vec<PartitionFiles> = partitions
            .into_iter()
            .map(|(key, (files, bytes))| PartitionFiles {
                spec_id: key.spec_id(),
                partition: partition_label(specs, &key),
                files,
                bytes,
            })
            .collect();
        top.sort_by(|left, right| {
            right
                .files
                .cmp(&left.files)
                .then_with(|| left.spec_id.cmp(&right.spec_id))
                .then_with(|| left.partition.cmp(&right.partition))
        });
        top.truncate(config.top_partitions);
        self.top_partitions = top;
        Ok(())
    }
}

/// The oldest and newest timestamps in the snapshot log, whatever order the
/// metadata file records them in.
fn snapshot_bounds(metadata: &TableMetadata) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let mut oldest = None;
    let mut newest = None;
    for entry in metadata.history() {
        let Some(at) = DateTime::from_timestamp_millis(entry.timestamp_ms) else {
            continue;
        };
        if oldest.is_none_or(|current: DateTime<Utc>| at < current) {
            oldest = Some(at);
        }
        if newest.is_none_or(|current: DateTime<Utc>| at > current) {
            newest = Some(at);
        }
    }
    (oldest, newest)
}

/// The target data-file size: the table's own property when it is a positive
/// integer, otherwise the configured default.
fn target_file_size(metadata: &TableMetadata, config: &StatisticsConfig) -> u64 {
    metadata
        .properties()
        .get(TARGET_FILE_SIZE_PROPERTY)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(config.default_target_file_size_bytes)
}

/// An empty histogram with one bucket per configured bound plus the open one.
fn empty_histogram(bounds: &[u64]) -> Vec<SizeBucket> {
    let mut buckets = Vec::with_capacity(bounds.len() + 1);
    let mut lower_bound_bytes = 0;
    for bound in bounds {
        buckets.push(SizeBucket {
            lower_bound_bytes,
            upper_bound_bytes: Some(*bound),
            files: 0,
            bytes: 0,
        });
        lower_bound_bytes = *bound;
    }
    buckets.push(SizeBucket {
        lower_bound_bytes,
        upper_bound_bytes: None,
        files: 0,
        bytes: 0,
    });
    buckets
}

/// Counts `size` into the bucket whose exclusive bound it falls below.
fn add_to_bucket(buckets: &mut [SizeBucket], bounds: &[u64], size: u64) {
    let index = bounds
        .iter()
        .position(|bound| size < *bound)
        .unwrap_or(bounds.len());
    if let Some(bucket) = buckets.get_mut(index) {
        bucket.files += 1;
        bucket.bytes = bucket.bytes.saturating_add(size);
    }
}

/// `field=value, field=value` for the key, or the empty string when the
/// partition has no fields.
fn partition_label(specs: &Specs, key: &PartitionKey) -> String {
    let Ok(bound) = specs.get(key.spec_id()) else {
        return String::new();
    };
    let mut parts = Vec::with_capacity(key.values().len());
    for (value, (name, primitive)) in key.values().iter().zip(&bound.fields) {
        let rendered = match value.bytes() {
            None => "null".to_owned(),
            Some(bytes) => Datum::try_from_bytes(bytes, primitive.clone()).map_or_else(
                |_| crate::hex::encode(bytes),
                |datum| literal_label(datum.literal()),
            ),
        };
        parts.push(format!("{name}={rendered}"));
    }
    parts.join(", ")
}

/// A partition value as text, without quoting strings.
fn literal_label(literal: &PrimitiveLiteral) -> String {
    match literal {
        PrimitiveLiteral::Boolean(value) => value.to_string(),
        PrimitiveLiteral::Int(value) => value.to_string(),
        PrimitiveLiteral::Long(value) => value.to_string(),
        PrimitiveLiteral::Float(value) => value.to_string(),
        PrimitiveLiteral::Double(value) => value.to_string(),
        PrimitiveLiteral::String(value) => value.clone(),
        PrimitiveLiteral::Binary(value) => crate::hex::encode(value),
        PrimitiveLiteral::Int128(value) => value.to_string(),
        PrimitiveLiteral::UInt128(value) => value.to_string(),
        PrimitiveLiteral::AboveMax => "above-max".to_owned(),
        PrimitiveLiteral::BelowMin => "below-min".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_cover_every_size_exactly_once() {
        let bounds = [10, 20];
        let mut buckets = empty_histogram(&bounds);
        for size in [0, 9, 10, 19, 20, 1_000] {
            add_to_bucket(&mut buckets, &bounds, size);
        }
        let counts: Vec<u64> = buckets.iter().map(|bucket| bucket.files).collect();
        assert_eq!(counts, [2, 2, 2]);
        assert_eq!(buckets[0].lower_bound_bytes, 0);
        assert_eq!(buckets[2].lower_bound_bytes, 20);
        assert_eq!(buckets[2].upper_bound_bytes, None);
    }

    #[test]
    fn configuration_rejects_unordered_bounds() {
        let config = StatisticsConfig {
            size_buckets_bytes: vec![8, 8],
            ..StatisticsConfig::default()
        };
        assert!(matches!(
            config.validate(),
            Err(StatisticsError::InvalidConfig { .. })
        ));
        let empty = StatisticsConfig {
            size_buckets_bytes: Vec::new(),
            ..StatisticsConfig::default()
        };
        assert!(matches!(
            empty.validate(),
            Err(StatisticsError::InvalidConfig { .. })
        ));
        assert!(StatisticsConfig::default().validate().is_ok());
    }
}
