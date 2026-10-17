//! The live files of the baseline snapshot, read from its manifests.
//!
//! Only the fields planning needs are kept, so memory grows with the number
//! of live files, which [`PlannerConfig::max_manifest_entries`] bounds.
//!
//! [`PlannerConfig::max_manifest_entries`]: crate::PlannerConfig::max_manifest_entries

use iceberg::io::FileIO;
use iceberg::spec::{
    DataContentType, DataFileFormat, Datum, FormatVersion, ManifestList, PrimitiveLiteral,
    TableMetadata,
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
        for manifest_file in list.entries() {
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
                let common = FileEntry {
                    path: file.file_path().to_owned(),
                    format: file.file_format(),
                    size: file.file_size_in_bytes(),
                    records: file.record_count(),
                    sequence_number,
                    partition: specs.key(spec_id, file.partition())?,
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
                            field_ids: file.equality_ids().unwrap_or_default(),
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
        let mut seen = std::collections::BTreeSet::new();
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
