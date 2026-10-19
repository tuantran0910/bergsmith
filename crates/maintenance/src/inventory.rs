//! The live files of the baseline snapshot, read from its manifests.
//!
//! Only the fields planning needs are kept, so memory grows with the number
//! of live files, which [`PlannerConfig::max_manifest_entries`] bounds.
//! Column metrics are kept only for fields that a live equality delete is
//! keyed on, because they decide which equality deletes may apply.
//!
//! [`PlannerConfig::max_manifest_entries`]: crate::PlannerConfig::max_manifest_entries

use std::collections::{BTreeMap, BTreeSet};

use iceberg::io::FileIO;
use iceberg::spec::{
    DataContentType, DataFile, DataFileFormat, Datum, FormatVersion, ManifestContentType,
    ManifestList, PrimitiveLiteral, TableMetadata,
};

use crate::baseline::PlanningBaseline;
use crate::error::PlanError;
use crate::partition::{PartitionKey, Specs};

/// The reserved field ID of a position delete's `file_path` column.
const FILE_PATH_FIELD_ID: i32 = 2_147_483_546;

/// What every live file records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileEntry {
    pub(crate) path: String,
    pub(crate) format: DataFileFormat,
    pub(crate) size: u64,
    pub(crate) records: u64,
    /// The data sequence number, after inheritance.
    pub(crate) sequence_number: i64,
    pub(crate) partition: PartitionKey,
    /// Recorded metrics of equality key fields, by field ID. A field without
    /// any recorded metric is absent.
    pub(crate) key_metrics: BTreeMap<i32, KeyMetrics>,
}

impl FileEntry {
    /// The recorded metrics of `field_id`; all unknown when none are recorded.
    pub(crate) fn metrics(&self, field_id: i32) -> KeyMetrics {
        self.key_metrics.get(&field_id).cloned().unwrap_or_default()
    }
}

/// What a manifest entry records about one column. Missing metrics are
/// unknown and never prove anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct KeyMetrics {
    /// The column's `null_value_counts` entry.
    pub(crate) nulls: Option<u64>,
    /// The column's lower bound.
    pub(crate) lower: Option<Datum>,
    /// The column's upper bound.
    pub(crate) upper: Option<Datum>,
}

impl KeyMetrics {
    /// The metrics `file` records for `field_id`, or `None` if it records none.
    fn of(file: &DataFile, field_id: i32) -> Option<Self> {
        let metrics = Self {
            nulls: file.null_value_counts().get(&field_id).copied(),
            lower: file.lower_bounds().get(&field_id).cloned(),
            upper: file.upper_bounds().get(&field_id).cloned(),
        };
        (metrics != Self::default()).then_some(metrics)
    }

    /// Whether the column may hold a null: unless a zero null count is recorded.
    pub(crate) fn may_contain_null(&self) -> bool {
        self.nulls.is_none_or(|count| count > 0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeleteKind {
    Position {
        /// The only data file the delete can apply to, when known from
        /// `referenced_data_file`.
        referenced: Option<String>,
        /// The `file_path` column's lower and upper bounds, when recorded.
        path_bounds: Option<(String, String)>,
    },
    Equality {
        field_ids: Vec<i32>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeleteEntry {
    pub(crate) file: FileEntry,
    pub(crate) kind: DeleteKind,
}

/// The live data and delete files of the baseline `main` snapshot, each
/// sorted by path, so manifest order never reaches a plan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Inventory {
    pub(crate) data: Vec<FileEntry>,
    pub(crate) deletes: Vec<DeleteEntry>,
}

impl Inventory {
    /// Reads the manifests of the baseline snapshot.
    pub(crate) async fn load(
        baseline: &PlanningBaseline,
        metadata: &TableMetadata,
        specs: &Specs,
        file_io: &FileIO,
        max_entries: u64,
    ) -> Result<Self, PlanError> {
        let snapshot =
            metadata
                .snapshot_by_id(baseline.snapshot_id())
                .ok_or(PlanError::MissingSnapshot {
                    snapshot_id: baseline.snapshot_id(),
                })?;
        let list = file_io.new_input(snapshot.manifest_list())?.read().await?;
        let list = ManifestList::parse_with_version(&list, FormatVersion::V2)?;
        let mut inventory = Self::default();
        let mut entries: u64 = 0;
        // Read delete manifests first, so every data entry keeps the metrics
        // of all live equality key fields and nothing else.
        let (delete_manifests, data_manifests): (Vec<_>, Vec<_>) = list
            .entries()
            .iter()
            .partition(|manifest| manifest.content == ManifestContentType::Deletes);
        let mut key_fields = BTreeSet::new();
        for manifest_file in delete_manifests.into_iter().chain(data_manifests) {
            let manifest = manifest_file.load_manifest(file_io).await?;
            for entry in manifest.entries().iter().filter(|entry| entry.is_alive()) {
                entries += 1;
                if entries > max_entries {
                    return Err(PlanError::InventoryTooLarge { limit: max_entries });
                }
                let file = entry.data_file();
                let sequence_number =
                    entry
                        .sequence_number()
                        .ok_or_else(|| PlanError::MissingSequenceNumber {
                            path: file.file_path().to_owned(),
                        })?;
                let spec_id = file.partition_spec_id();
                let equality_ids = file.equality_ids().unwrap_or_default();
                let fields: Vec<i32> = match file.content_type() {
                    DataContentType::Data => key_fields.iter().copied().collect(),
                    DataContentType::PositionDeletes => Vec::new(),
                    DataContentType::EqualityDeletes => {
                        key_fields.extend(equality_ids.iter().copied());
                        equality_ids.clone()
                    }
                };
                let common = FileEntry {
                    path: file.file_path().to_owned(),
                    format: file.file_format(),
                    size: file.file_size_in_bytes(),
                    records: file.record_count(),
                    sequence_number,
                    partition: specs.key(spec_id, file.partition())?,
                    key_metrics: fields
                        .into_iter()
                        .filter_map(|id| Some((id, KeyMetrics::of(file, id)?)))
                        .collect(),
                };
                match file.content_type() {
                    DataContentType::Data => inventory.data.push(common),
                    DataContentType::PositionDeletes => inventory.deletes.push(DeleteEntry {
                        file: common,
                        kind: DeleteKind::Position {
                            referenced: file.referenced_data_file(),
                            path_bounds: string_bound(file.lower_bounds().get(&FILE_PATH_FIELD_ID))
                                .zip(string_bound(file.upper_bounds().get(&FILE_PATH_FIELD_ID))),
                        },
                    }),
                    DataContentType::EqualityDeletes => inventory.deletes.push(DeleteEntry {
                        file: common,
                        kind: DeleteKind::Equality {
                            field_ids: equality_ids,
                        },
                    }),
                }
            }
        }
        Self::from_entries(inventory.data, inventory.deletes)
    }

    /// An inventory of `data` and `deletes` in any order.
    pub(crate) fn from_entries(
        mut data: Vec<FileEntry>,
        mut deletes: Vec<DeleteEntry>,
    ) -> Result<Self, PlanError> {
        data.sort_by(|a, b| a.path.cmp(&b.path));
        deletes.sort_by(|a, b| a.file.path.cmp(&b.file.path));
        let paths = data
            .iter()
            .map(|file| &file.path)
            .chain(deletes.iter().map(|delete| &delete.file.path));
        let mut seen = BTreeSet::new();
        for path in paths {
            if !seen.insert(path) {
                return Err(PlanError::DuplicateFile { path: path.clone() });
            }
        }
        Ok(Self { data, deletes })
    }
}

fn string_bound(datum: Option<&Datum>) -> Option<String> {
    match datum?.literal() {
        PrimitiveLiteral::String(value) => Some(value.clone()),
        _ => None,
    }
}
