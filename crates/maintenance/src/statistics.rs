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
    DataContentType, Datum, FormatVersion, ManifestList, PrimitiveLiteral, Snapshot, TableMetadata,
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
    /// How many of the newest snapshots one collection pass records for the
    /// table explorer's timeline. Older snapshots stay in the table's own
    /// metadata and are picked up by a later pass if it retains them.
    #[serde(default = "default_snapshot_history")]
    pub snapshot_history: usize,
}

impl Default for StatisticsConfig {
    fn default() -> Self {
        Self {
            max_manifest_entries: 200_000,
            size_buckets_bytes: vec![MIB, 8 * MIB, 32 * MIB, 128 * MIB, 512 * MIB],
            default_target_file_size_bytes: 512 * MIB,
            top_partitions: 10,
            snapshot_history: 500,
        }
    }
}

/// The snapshot history bound used when a configuration does not set one.
const fn default_snapshot_history() -> usize {
    500
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
        if !(1..=10_000).contains(&self.snapshot_history) {
            return Err(StatisticsError::InvalidConfig {
                reason: "snapshot_history must be between 1 and 10000",
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

/// One partition of the current snapshot: its data files and the delete files
/// that apply to it. Values use the spec's field names and the Iceberg
/// single-value encoding decoded back to text.
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
    /// Delete files in the partition, of either kind. Zero for samples stored
    /// before the field existed; a partition without data files is not listed,
    /// and its delete files still count toward [`TableSample::delete_files`].
    #[serde(default)]
    pub delete_files: u64,
}

/// Counts one partition's live files while a sample walks manifests.
#[derive(Default)]
struct PartitionTally {
    files: u64,
    bytes: u64,
    delete_files: u64,
}

/// One commit in a table's snapshot log, as its metadata records it.
///
/// The summary properties are the committing engine's own: Iceberg's change
/// counters, whatever the engine adds, and — on a Bergsmith publication — the
/// `bergsmith.job-id`, `bergsmith.publication-id`, and
/// `bergsmith.proposal-sha256` evidence. The timeline links a commit to its
/// job through that property, without a second ledger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotEntry {
    /// The snapshot ID.
    pub snapshot_id: i64,
    /// The snapshot this commit replaced, if any.
    pub parent_snapshot_id: Option<i64>,
    /// The sequence number the commit was assigned.
    pub sequence_number: i64,
    /// When the commit happened.
    pub committed_at: DateTime<Utc>,
    /// The operation, such as `append`, `overwrite`, `replace`, or `delete`.
    pub operation: String,
    /// Every summary property the committing engine wrote, in key order.
    pub properties: BTreeMap<String, String>,
}

/// The change counts a snapshot's summary records for its own commit.
///
/// A property the engine did not write counts as zero, which is what the
/// summary means by omission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotDeltas {
    /// Data files added by the commit.
    pub added_data_files: u64,
    /// Data files the commit deleted.
    pub deleted_data_files: u64,
    /// Rows in the data files the commit added.
    pub added_records: u64,
    /// Rows in the data files the commit deleted.
    pub deleted_records: u64,
    /// Bytes of the data files the commit added.
    pub added_bytes: u64,
    /// Bytes of the data files the commit removed.
    pub removed_bytes: u64,
    /// Delete files the commit added, of either kind.
    pub added_delete_files: u64,
    /// Delete files the commit removed, of either kind.
    pub removed_delete_files: u64,
    /// Rows recorded by the position delete files the commit added.
    pub added_position_deletes: u64,
    /// Rows recorded by the position delete files the commit removed.
    pub removed_position_deletes: u64,
    /// Rows recorded by the equality delete files the commit added.
    pub added_equality_deletes: u64,
    /// Rows recorded by the equality delete files the commit removed.
    pub removed_equality_deletes: u64,
}

impl SnapshotDeltas {
    /// Reads the change counts from a snapshot's summary properties.
    #[must_use]
    pub fn from_properties(properties: &BTreeMap<String, String>) -> Self {
        let count = |key: &str| -> u64 {
            properties
                .get(key)
                .and_then(|value| value.parse().ok())
                .unwrap_or(0)
        };
        Self {
            added_data_files: count("added-data-files"),
            deleted_data_files: count("deleted-data-files"),
            added_records: count("added-records"),
            deleted_records: count("deleted-records"),
            added_bytes: count("added-files-size"),
            removed_bytes: count("removed-files-size"),
            added_delete_files: count("added-delete-files"),
            removed_delete_files: count("removed-delete-files"),
            added_position_deletes: count("added-position-deletes"),
            removed_position_deletes: count("removed-position-deletes"),
            added_equality_deletes: count("added-equality-deletes"),
            removed_equality_deletes: count("removed-equality-deletes"),
        }
    }
}

/// The newest `limit` entries of `metadata`'s snapshot log, newest first.
///
/// A snapshot whose timestamp does not fit the calendar is skipped; two
/// commits at the same instant keep a stable order by snapshot ID.
///
/// Ordering costs O(N log N) in the table's retained snapshots, and the
/// reference vector is O(N). Only the kept entries have their summaries
/// cloned, so the allocation that grows with the content of the history is
/// bounded by `limit` rather than by the table's whole past.
#[must_use]
pub fn snapshot_history(metadata: &TableMetadata, limit: usize) -> Vec<SnapshotEntry> {
    let mut newest: Vec<(DateTime<Utc>, &Snapshot)> = metadata
        .snapshots()
        .filter_map(|snapshot| {
            let committed_at = DateTime::from_timestamp_millis(snapshot.timestamp_ms())?;
            Some((committed_at, snapshot.as_ref()))
        })
        .collect();
    newest.sort_unstable_by(|(left_at, left), (right_at, right)| {
        right_at
            .cmp(left_at)
            .then_with(|| right.snapshot_id().cmp(&left.snapshot_id()))
    });
    newest.truncate(limit);
    newest
        .into_iter()
        .map(|(committed_at, snapshot)| SnapshotEntry {
            snapshot_id: snapshot.snapshot_id(),
            parent_snapshot_id: snapshot.parent_snapshot_id(),
            sequence_number: snapshot.sequence_number(),
            committed_at,
            operation: snapshot.summary().operation.as_str().to_owned(),
            properties: snapshot
                .summary()
                .additional_properties
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        })
        .collect()
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
    /// The busiest partitions by data-file count, each with the delete files
    /// that apply to it, bounded by [`StatisticsConfig::top_partitions`].
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
        let mut partitions: BTreeMap<PartitionKey, PartitionTally> = BTreeMap::new();
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
                        let partition = partitions.entry(key).or_default();
                        partition.files += 1;
                        partition.bytes = partition.bytes.saturating_add(size);
                    }
                    DataContentType::PositionDeletes => {
                        self.delete_files += 1;
                        self.delete_bytes = self.delete_bytes.saturating_add(size);
                        self.position_delete_files += 1;
                        let key = specs.key(file.partition_spec_id(), file.partition())?;
                        partitions.entry(key).or_default().delete_files += 1;
                        self.position_delete_records =
                            self.position_delete_records.saturating_add(records);
                    }
                    DataContentType::EqualityDeletes => {
                        self.delete_files += 1;
                        self.delete_bytes = self.delete_bytes.saturating_add(size);
                        self.equality_delete_files += 1;
                        let key = specs.key(file.partition_spec_id(), file.partition())?;
                        partitions.entry(key).or_default().delete_files += 1;
                        self.equality_delete_records =
                            self.equality_delete_records.saturating_add(records);
                    }
                }
            }
        }

        self.partition_count =
            u64::try_from(partitions.values().filter(|tally| tally.files > 0).count())
                .unwrap_or(u64::MAX);
        let mut top: Vec<PartitionFiles> = partitions
            .into_iter()
            .filter(|(_, tally)| tally.files > 0)
            .map(|(key, tally)| PartitionFiles {
                spec_id: key.spec_id(),
                partition: partition_label(specs, &key),
                files: tally.files,
                bytes: tally.bytes,
                delete_files: tally.delete_files,
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

    #[test]
    fn configuration_rejects_an_unusable_snapshot_history() {
        for snapshot_history in [0, 10_001] {
            let config = StatisticsConfig {
                snapshot_history,
                ..StatisticsConfig::default()
            };
            assert!(matches!(
                config.validate(),
                Err(StatisticsError::InvalidConfig { .. })
            ));
        }
    }

    #[test]
    fn a_configuration_without_the_snapshot_bound_keeps_the_default() {
        let config: StatisticsConfig = serde_json::from_str(
            r#"{
                "max_manifest_entries": 10,
                "size_buckets_bytes": [1024],
                "default_target_file_size_bytes": 1024,
                "top_partitions": 1
            }"#,
        )
        .expect("a configuration written before the snapshot bound must still parse");
        assert_eq!(config.snapshot_history, 500);
    }

    #[test]
    fn a_partition_stored_before_delete_counts_reads_as_zero() {
        let partition: PartitionFiles =
            serde_json::from_str(r#"{"spec_id":0,"partition":"tag=x","files":2,"bytes":2048}"#)
                .expect("a sample written before the delete count must still parse");
        assert_eq!(partition.delete_files, 0);
    }

    #[test]
    fn deltas_read_only_the_change_counters() {
        let properties = BTreeMap::from([
            ("added-data-files".to_owned(), "3".to_owned()),
            ("deleted-data-files".to_owned(), "1".to_owned()),
            ("added-records".to_owned(), "30".to_owned()),
            ("deleted-records".to_owned(), "10".to_owned()),
            ("added-delete-files".to_owned(), "2".to_owned()),
            ("total-records".to_owned(), "20".to_owned()),
            ("engine-name".to_owned(), "spark".to_owned()),
            ("added-records-bogus".to_owned(), "999".to_owned()),
        ]);
        let delta = SnapshotDeltas::from_properties(&properties);
        assert_eq!(delta.added_data_files, 3);
        assert_eq!(delta.deleted_data_files, 1);
        assert_eq!(delta.added_records, 30);
        assert_eq!(delta.deleted_records, 10);
        assert_eq!(delta.added_delete_files, 2);
        assert_eq!(delta.removed_delete_files, 0);
        assert_eq!(delta.added_bytes, 0);
    }

    #[test]
    fn an_unreadable_counter_counts_as_zero() {
        let properties = BTreeMap::from([("added-records".to_owned(), "many".to_owned())]);
        let delta = SnapshotDeltas::from_properties(&properties);
        assert_eq!(delta.added_records, 0);
    }
}
