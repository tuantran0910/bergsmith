//! Metadata-only rewrites of `main`: manifest rewrite and dangling-delete
//! removal (BS-031.T3, ADR 0063).
//!
//! Both operations read the baseline `main` snapshot's manifest list (and, for
//! dangling deletes, its manifests) and decide what one `replace` snapshot
//! should change. Nothing here writes or commits: the coordinator stores the
//! [`MetadataPlanDocument`] and publishes it through the capture catalog.
//!
//! - Manifest rewrite selects the data manifests smaller than the target
//!   manifest size, per partition spec, where a spec has at least two to merge.
//! - A delete file is dangling when the planner's v2 delete index finds no live
//!   data file in its scope. Recorded column bounds count as proof, as they do
//!   for the reader, except for floating-point equality keys: NaN has no place
//!   in an ordering, so those deletes are judged by scope and sequence numbers.

use std::collections::{BTreeMap, BTreeSet};

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{JobId, PlanId};
use iceberg::io::FileIO;
use iceberg::spec::{
    FormatVersion, ManifestContentType, ManifestFile, ManifestList, PrimitiveType, TableMetadata,
    Type,
};
use serde::{Deserialize, Serialize};

use crate::baseline::PlanningBaseline;
use crate::deletes::PendingDeletes;
use crate::error::PlanError;
use crate::inventory::{DeleteKind, Inventory};
use crate::partition::Specs;

/// The hash domain of a metadata plan document.
const METADATA_PLAN_DOMAIN: &[u8] = b"bergsmith.maintenance.v1.MetadataPlan";

/// The metadata plan document layout version.
pub const METADATA_PLAN_VERSION: u32 = 1;

/// The table property that sizes rewritten manifests.
pub const MANIFEST_TARGET_SIZE_PROPERTY: &str = "commit.manifest.target-size-bytes";

/// Iceberg's default target manifest size, 8 MiB.
pub const DEFAULT_MANIFEST_TARGET_SIZE_BYTES: u64 = 8 * 1024 * 1024;

/// The fewest manifests of one spec worth merging.
const MIN_MANIFESTS_PER_SPEC: usize = 2;

/// Why a metadata plan could not be built.
#[derive(Debug, thiserror::Error)]
pub enum MetadataPlanError {
    /// Only v2 tables are maintained.
    #[error("table format version {version} is not supported")]
    FormatVersion {
        /// The table's format version.
        version: i32,
    },
    /// A bound of the operation is zero.
    #[error("invalid metadata plan bounds: {0}")]
    InvalidBounds(&'static str),
    /// Reading the metadata, a manifest list, or a manifest failed.
    #[error(transparent)]
    Plan(#[from] PlanError),
}

impl From<iceberg::Error> for MetadataPlanError {
    fn from(error: iceberg::Error) -> Self {
        Self::Plan(PlanError::Iceberg(error))
    }
}

/// One data manifest a manifest rewrite replaces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedManifest {
    /// The manifest file location.
    pub path: String,
    /// The manifest file length in bytes.
    pub length: u64,
    /// The partition spec of every entry.
    pub spec_id: i32,
    /// Live (added plus existing) data files the manifest records.
    pub live_files: u64,
}

/// The kind of a planned delete file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteContent {
    /// A position delete file.
    Position,
    /// An equality delete file.
    Equality,
}

/// One delete file that applies to no live data file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedDanglingDelete {
    /// The delete file location.
    pub path: String,
    /// Position or equality.
    pub content: DeleteContent,
    /// The file size in bytes.
    pub size: u64,
    /// The number of delete records.
    pub records: u64,
    /// The data sequence number, after inheritance.
    pub sequence_number: i64,
}

/// What a metadata plan changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetadataWork {
    /// Merge small data manifests into target-sized ones.
    RewriteManifests {
        /// The target output manifest size.
        target_size_bytes: u64,
        /// The manifests to replace, by spec then path.
        manifests: Vec<PlannedManifest>,
        /// Eligible manifests left for a later job by the input bounds.
        deferred: u64,
    },
    /// Remove delete files that apply to no live data file.
    RemoveDanglingDeletes {
        /// The delete files to remove, by path.
        deletes: Vec<PlannedDanglingDelete>,
        /// Live delete files examined.
        examined: u64,
        /// Dangling delete files left for a later job by the removal bound.
        deferred: u64,
    },
}

/// The bounds of a manifest rewrite selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestPolicy {
    /// The most manifests one job replaces.
    pub max_input_manifests: u64,
    /// The most live data files the replaced manifests may record.
    pub max_input_entries: u64,
}

/// The manifests a rewrite of `main` would replace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestSelection {
    /// The target output manifest size.
    pub target_size_bytes: u64,
    /// Selected manifests, by spec then path.
    pub manifests: Vec<PlannedManifest>,
    /// Eligible manifests the bounds left out.
    pub deferred: u64,
}

impl ManifestSelection {
    /// The plan content for this selection.
    #[must_use]
    pub fn into_work(self) -> MetadataWork {
        MetadataWork::RewriteManifests {
            target_size_bytes: self.target_size_bytes,
            manifests: self.manifests,
            deferred: self.deferred,
        }
    }
}

/// The dangling delete files of `main`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DanglingSelection {
    /// Selected delete files, by path.
    pub deletes: Vec<PlannedDanglingDelete>,
    /// Live delete files examined.
    pub examined: u64,
    /// Dangling delete files the removal bound left out.
    pub deferred: u64,
}

impl DanglingSelection {
    /// The plan content for this selection.
    #[must_use]
    pub fn into_work(self) -> MetadataWork {
        MetadataWork::RemoveDanglingDeletes {
            deletes: self.deletes,
            examined: self.examined,
            deferred: self.deferred,
        }
    }
}

/// The target manifest size of `metadata`: its
/// [`MANIFEST_TARGET_SIZE_PROPERTY`], or 8 MiB when unset, unparsable, or
/// zero, as iceberg-rust's rewrite action resolves it.
#[must_use]
pub fn manifest_target_size(metadata: &TableMetadata) -> u64 {
    metadata
        .properties()
        .get(MANIFEST_TARGET_SIZE_PROPERTY)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|size| *size > 0)
        .unwrap_or(DEFAULT_MANIFEST_TARGET_SIZE_BYTES)
}

fn require_v2(metadata: &TableMetadata) -> Result<(), MetadataPlanError> {
    if metadata.format_version() == FormatVersion::V2 {
        Ok(())
    } else {
        Err(MetadataPlanError::FormatVersion {
            version: i32::from(metadata.format_version() as u8),
        })
    }
}

/// Selects the data manifests of the baseline snapshot to rewrite.
///
/// Reads only the manifest list. The caller bounds the read with a deadline.
///
/// # Errors
///
/// [`MetadataPlanError::FormatVersion`] for a table that is not v2,
/// [`MetadataPlanError::InvalidBounds`] for a zero bound, and
/// [`MetadataPlanError::Plan`] when the baseline snapshot or its manifest list
/// cannot be read.
pub async fn select_manifests(
    metadata: &TableMetadata,
    baseline: &PlanningBaseline,
    file_io: &FileIO,
    policy: ManifestPolicy,
) -> Result<ManifestSelection, MetadataPlanError> {
    require_v2(metadata)?;
    let snapshot =
        metadata
            .snapshot_by_id(baseline.snapshot_id())
            .ok_or(PlanError::MissingSnapshot {
                snapshot_id: baseline.snapshot_id(),
            })?;
    let bytes = file_io.new_input(snapshot.manifest_list())?.read().await?;
    let list = ManifestList::parse_with_version(&bytes, FormatVersion::V2)?;
    choose_manifests(list.entries(), manifest_target_size(metadata), policy)
}

/// The manifests of `entries` worth rewriting under `policy`.
fn choose_manifests(
    entries: &[ManifestFile],
    target_size_bytes: u64,
    policy: ManifestPolicy,
) -> Result<ManifestSelection, MetadataPlanError> {
    if policy.max_input_manifests == 0 || policy.max_input_entries == 0 {
        return Err(MetadataPlanError::InvalidBounds(
            "manifest rewrite bounds must be positive",
        ));
    }
    let mut by_spec: BTreeMap<i32, Vec<PlannedManifest>> = BTreeMap::new();
    for manifest in entries {
        if manifest.content != ManifestContentType::Data
            || manifest.manifest_length < 0
            || manifest.manifest_length.unsigned_abs() >= target_size_bytes
        {
            continue;
        }
        // The rewrite action cannot verify a manifest without counts.
        let (Some(added), Some(existing)) =
            (manifest.added_files_count, manifest.existing_files_count)
        else {
            continue;
        };
        by_spec
            .entry(manifest.partition_spec_id)
            .or_default()
            .push(PlannedManifest {
                path: manifest.manifest_path.clone(),
                length: manifest.manifest_length.unsigned_abs(),
                spec_id: manifest.partition_spec_id,
                live_files: u64::from(added) + u64::from(existing),
            });
    }
    let mut eligible: u64 = 0;
    let mut manifests = Vec::new();
    let (mut taken, mut entries_taken) = (0_u64, 0_u64);
    for (_, mut group) in by_spec {
        if group.len() < MIN_MANIFESTS_PER_SPEC {
            continue;
        }
        group.sort_by(|a, b| a.path.cmp(&b.path));
        eligible += group.len() as u64;
        let mut chosen = Vec::new();
        for manifest in group {
            let entries = entries_taken.saturating_add(manifest.live_files);
            if taken < policy.max_input_manifests && entries <= policy.max_input_entries {
                taken += 1;
                entries_taken = entries;
                chosen.push(manifest);
            }
        }
        if chosen.len() < MIN_MANIFESTS_PER_SPEC {
            // One manifest of a spec has nothing to merge with; release it.
            taken -= chosen.len() as u64;
            entries_taken -= chosen.iter().map(|m| m.live_files).sum::<u64>();
            continue;
        }
        manifests.extend(chosen);
    }
    Ok(ManifestSelection {
        target_size_bytes,
        deferred: eligible - manifests.len() as u64,
        manifests,
    })
}

/// Selects the dangling delete files of the baseline snapshot.
///
/// Reads every manifest of the baseline snapshot, up to `max_entries` live
/// entries, then classifies the deletes. Classification yields regularly, so
/// the caller bounds both the reads and the classification with a deadline.
///
/// # Errors
///
/// [`MetadataPlanError::FormatVersion`] for a table that is not v2,
/// [`MetadataPlanError::InvalidBounds`] for a zero bound, and
/// [`MetadataPlanError::Plan`] when the manifests cannot be read or exceed
/// `max_entries`.
pub async fn select_dangling_deletes(
    metadata: &TableMetadata,
    baseline: &PlanningBaseline,
    file_io: &FileIO,
    max_entries: u64,
    max_removals: u64,
) -> Result<DanglingSelection, MetadataPlanError> {
    require_v2(metadata)?;
    if max_entries == 0 || max_removals == 0 {
        return Err(MetadataPlanError::InvalidBounds(
            "dangling-delete bounds must be positive",
        ));
    }
    let specs = Specs::of(metadata)?;
    let inventory = Inventory::load(baseline, metadata, &specs, file_io, max_entries).await?;
    Ok(dangling(
        inventory,
        &specs,
        &floating_point_fields(metadata),
        max_removals,
    )
    .await)
}

/// Field IDs that are `float` or `double` in any schema of `metadata`.
fn floating_point_fields(metadata: &TableMetadata) -> BTreeSet<i32> {
    let mut fields = BTreeSet::new();
    for schema in metadata.schemas_iter() {
        for (id, field) in schema.field_id_to_fields() {
            if matches!(
                field.field_type.as_ref(),
                Type::Primitive(PrimitiveType::Float | PrimitiveType::Double)
            ) {
                fields.insert(*id);
            }
        }
    }
    fields
}

/// Delete-applicability tests between cooperative yields.
const TESTS_PER_YIELD: usize = 4096;

/// The dangling deletes of `inventory`, by path, at most `max_removals`.
///
/// Yields every [`TESTS_PER_YIELD`] applicability tests, so a deadline around
/// the returned future can stop it.
async fn dangling(
    mut inventory: Inventory,
    specs: &Specs,
    floating: &BTreeSet<i32>,
    max_removals: u64,
) -> DanglingSelection {
    // Bounds on a floating-point key prove nothing: forget them, so only
    // scope and sequence numbers can exclude the delete.
    for delete in &mut inventory.deletes {
        if let DeleteKind::Equality { field_ids } = &delete.kind {
            for field_id in field_ids {
                if floating.contains(field_id) {
                    delete.file.key_metrics.remove(field_id);
                }
            }
        }
    }
    let mut pending = PendingDeletes::new(&inventory.deletes, specs);
    let mut tested = 0;
    for data in &inventory.data {
        if pending.is_empty() {
            break;
        }
        tested += pending.discharge(data);
        // Classification is CPU work: yield regularly so the caller's
        // deadline can cancel it and the runtime worker stays responsive.
        if tested >= TESTS_PER_YIELD {
            tested = 0;
            tokio::task::yield_now().await;
        }
    }
    let applied = pending.into_applied();
    let mut deletes: Vec<PlannedDanglingDelete> = inventory
        .deletes
        .iter()
        .filter(|delete| !applied.contains(delete.file.path.as_str()))
        .map(|delete| PlannedDanglingDelete {
            path: delete.file.path.clone(),
            content: match delete.kind {
                DeleteKind::Position { .. } => DeleteContent::Position,
                DeleteKind::Equality { .. } => DeleteContent::Equality,
            },
            size: delete.file.size,
            records: delete.file.records,
            sequence_number: delete.file.sequence_number,
        })
        .collect();
    // The inventory is sorted by path, so truncation keeps a stable prefix.
    let total = deletes.len() as u64;
    deletes.truncate(usize::try_from(max_removals).unwrap_or(usize::MAX));
    DanglingSelection {
        deferred: total - deletes.len() as u64,
        examined: inventory.deletes.len() as u64,
        deletes,
    }
}

/// An immutable metadata-only plan for one job.
///
/// The canonical JSON is the serialized typed form; the hash is
/// `SHA-256("bergsmith.maintenance.v1.MetadataPlan" || 0x00 || JSON)`, as for
/// the other plan documents. PostgreSQL's `jsonb` reorders keys, so reading a
/// stored document re-serializes the typed form before comparing hashes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataPlanDocument {
    /// The document layout version; always [`METADATA_PLAN_VERSION`].
    pub version: u32,
    /// The job, which must be in `Planning` when the document is stored.
    pub job_id: JobId,
    /// The plan's identifier.
    pub plan_id: PlanId,
    /// The table state the selection was made from.
    pub baseline: PlanningBaseline,
    /// What the publication changes.
    pub work: MetadataWork,
}

impl MetadataPlanDocument {
    /// Assembles a document.
    ///
    /// # Errors
    ///
    /// [`PlanError::InvalidConfig`] when the work is empty or names a path
    /// twice: such a plan could not publish.
    pub fn new(
        job_id: JobId,
        plan_id: PlanId,
        baseline: PlanningBaseline,
        work: MetadataWork,
    ) -> Result<Self, PlanError> {
        let paths: Vec<&str> = match &work {
            MetadataWork::RewriteManifests { manifests, .. } => {
                manifests.iter().map(|m| m.path.as_str()).collect()
            }
            MetadataWork::RemoveDanglingDeletes { deletes, .. } => {
                deletes.iter().map(|d| d.path.as_str()).collect()
            }
        };
        if paths.is_empty() {
            return Err(PlanError::InvalidConfig(
                "a metadata plan must change at least one file".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        if let Some(path) = paths.iter().find(|path| !seen.insert(**path)) {
            return Err(PlanError::InvalidConfig(format!(
                "a metadata plan names {path} twice"
            )));
        }
        Ok(Self {
            version: METADATA_PLAN_VERSION,
            job_id,
            plan_id,
            baseline,
            work,
        })
    }

    /// The canonical JSON of the document.
    ///
    /// # Errors
    ///
    /// [`PlanError::CorruptPlan`] if serialization fails, which a document
    /// built by [`new`](Self::new) never does.
    pub fn to_json(&self) -> Result<Vec<u8>, PlanError> {
        serde_json::to_vec(self).map_err(|error| {
            PlanError::CorruptPlan(format!("the metadata plan does not encode: {error}"))
        })
    }

    /// The document as a JSON value, for storage.
    ///
    /// # Errors
    ///
    /// As for [`to_json`](Self::to_json).
    pub fn to_value(&self) -> Result<serde_json::Value, PlanError> {
        serde_json::to_value(self).map_err(|error| {
            PlanError::CorruptPlan(format!("the metadata plan does not encode: {error}"))
        })
    }

    /// The document hash.
    ///
    /// # Errors
    ///
    /// As for [`to_json`](Self::to_json).
    pub fn content_hash(&self) -> Result<Sha256Digest, PlanError> {
        Ok(Sha256Digest::of(METADATA_PLAN_DOMAIN, &self.to_json()?))
    }

    /// Reads a persisted document. The caller compares the stored hash.
    ///
    /// # Errors
    ///
    /// [`PlanError::CorruptPlan`] when the content does not decode or its
    /// version is not [`METADATA_PLAN_VERSION`].
    pub fn from_persisted(value: serde_json::Value) -> Result<Self, PlanError> {
        let document: Self = serde_json::from_value(value).map_err(|error| {
            PlanError::CorruptPlan(format!("the metadata plan does not decode: {error}"))
        })?;
        if document.version != METADATA_PLAN_VERSION {
            return Err(PlanError::CorruptPlan(format!(
                "metadata plan version {} is not supported",
                document.version
            )));
        }
        Ok(document)
    }
}

#[cfg(test)]
mod tests {
    use iceberg::spec::{Datum, ManifestContentType};

    use super::*;
    use crate::deletes::tests::file;
    use crate::inventory::{DeleteEntry, FileEntry, KeyMetrics};
    use crate::partition::PartitionKey;

    fn manifest(
        path: &str,
        length: i64,
        spec_id: i32,
        content: ManifestContentType,
    ) -> ManifestFile {
        ManifestFile {
            manifest_path: path.to_owned(),
            manifest_length: length,
            partition_spec_id: spec_id,
            content,
            sequence_number: 1,
            min_sequence_number: 1,
            added_snapshot_id: 1,
            added_files_count: Some(2),
            existing_files_count: Some(1),
            deleted_files_count: Some(0),
            added_rows_count: Some(2),
            existing_rows_count: Some(1),
            deleted_rows_count: Some(0),
            partitions: None,
            key_metadata: None,
            first_row_id: None,
        }
    }

    fn policy(max_input_manifests: u64, max_input_entries: u64) -> ManifestPolicy {
        ManifestPolicy {
            max_input_manifests,
            max_input_entries,
        }
    }

    fn paths(selection: &ManifestSelection) -> Vec<&str> {
        selection
            .manifests
            .iter()
            .map(|m| m.path.as_str())
            .collect()
    }

    #[test]
    fn small_data_manifests_of_one_spec_are_selected_in_path_order() {
        let entries = [
            manifest("m3", 10, 0, ManifestContentType::Data),
            manifest("m1", 10, 0, ManifestContentType::Data),
            manifest("big", 100, 0, ManifestContentType::Data),
            manifest("d1", 10, 0, ManifestContentType::Deletes),
            manifest("d2", 10, 0, ManifestContentType::Deletes),
        ];
        let selection = choose_manifests(&entries, 100, policy(10, 100)).unwrap();
        assert_eq!(paths(&selection), ["m1", "m3"]);
        assert_eq!(selection.manifests[0].live_files, 3);
        assert_eq!(selection.deferred, 0);
    }

    #[test]
    fn a_spec_with_one_small_manifest_has_nothing_to_merge() {
        let entries = [
            manifest("a", 10, 0, ManifestContentType::Data),
            manifest("b", 10, 1, ManifestContentType::Data),
            manifest("c", 10, 1, ManifestContentType::Data),
        ];
        let selection = choose_manifests(&entries, 100, policy(10, 100)).unwrap();
        assert_eq!(paths(&selection), ["b", "c"]);
    }

    #[test]
    fn manifests_without_counts_are_never_selected() {
        let mut unknown = manifest("a", 10, 0, ManifestContentType::Data);
        unknown.existing_files_count = None;
        let entries = [
            unknown,
            manifest("b", 10, 0, ManifestContentType::Data),
            manifest("c", 10, 0, ManifestContentType::Data),
        ];
        let selection = choose_manifests(&entries, 100, policy(10, 100)).unwrap();
        assert_eq!(paths(&selection), ["b", "c"]);
    }

    #[test]
    fn bounds_defer_the_rest_and_never_leave_a_lone_manifest() {
        let entries = [
            manifest("a1", 10, 0, ManifestContentType::Data),
            manifest("a2", 10, 0, ManifestContentType::Data),
            manifest("a3", 10, 0, ManifestContentType::Data),
            manifest("b1", 10, 1, ManifestContentType::Data),
            manifest("b2", 10, 1, ManifestContentType::Data),
        ];
        let by_count = choose_manifests(&entries, 100, policy(4, 100)).unwrap();
        assert_eq!(paths(&by_count), ["a1", "a2", "a3"]);
        assert_eq!(by_count.deferred, 2);

        // Each manifest records three live files; nine fit.
        let by_entries = choose_manifests(&entries, 100, policy(10, 9)).unwrap();
        assert_eq!(paths(&by_entries), ["a1", "a2", "a3"]);
        assert_eq!(by_entries.deferred, 2);

        let none = choose_manifests(&entries, 100, policy(1, 100)).unwrap();
        assert!(none.manifests.is_empty());
        assert_eq!(none.deferred, 5);
    }

    #[test]
    fn zero_bounds_are_rejected() {
        assert!(matches!(
            choose_manifests(&[], 100, policy(0, 1)),
            Err(MetadataPlanError::InvalidBounds(_))
        ));
        assert!(matches!(
            choose_manifests(&[], 100, policy(1, 0)),
            Err(MetadataPlanError::InvalidBounds(_))
        ));
    }

    fn specs() -> Specs {
        let schema = iceberg::spec::Schema::builder()
            .with_fields([
                std::sync::Arc::new(iceberg::spec::NestedField::required(
                    1,
                    "id",
                    Type::Primitive(PrimitiveType::Long),
                )),
                std::sync::Arc::new(iceberg::spec::NestedField::required(
                    2,
                    "score",
                    Type::Primitive(PrimitiveType::Double),
                )),
            ])
            .build()
            .unwrap();
        let metadata = iceberg::spec::TableMetadataBuilder::new(
            schema,
            iceberg::spec::UnboundPartitionSpec::builder().build(),
            iceberg::spec::SortOrder::unsorted_order(),
            "file:///t".to_owned(),
            FormatVersion::V2,
            std::collections::HashMap::new(),
        )
        .unwrap()
        .build()
        .unwrap()
        .metadata;
        assert_eq!(floating_point_fields(&metadata), BTreeSet::from([2]));
        Specs::of(&metadata).unwrap()
    }

    fn delete(path: &str, sequence_number: i64, kind: DeleteKind) -> DeleteEntry {
        DeleteEntry {
            file: file(path, sequence_number, PartitionKey::unpartitioned()),
            kind,
        }
    }

    fn bounded(mut entry: FileEntry, field_id: i32, lower: Datum, upper: Datum) -> FileEntry {
        entry.key_metrics.insert(
            field_id,
            KeyMetrics {
                nulls: Some(0),
                lower: Some(lower),
                upper: Some(upper),
            },
        );
        entry
    }

    fn select(data: Vec<FileEntry>, deletes: Vec<DeleteEntry>, max: u64) -> DanglingSelection {
        let inventory = Inventory::from_entries(data, deletes).unwrap();
        futures::executor::block_on(dangling(inventory, &specs(), &BTreeSet::from([2]), max))
    }

    fn dangling_paths(selection: &DanglingSelection) -> Vec<&str> {
        selection.deletes.iter().map(|d| d.path.as_str()).collect()
    }

    fn untargeted() -> DeleteKind {
        DeleteKind::Position {
            referenced: None,
            path_bounds: None,
        }
    }

    fn global_deletes(count: usize) -> (Vec<FileEntry>, Vec<DeleteEntry>) {
        let data = (0..count)
            .map(|i| file(&format!("d{i:05}"), 1, PartitionKey::unpartitioned()))
            .collect();
        let deletes = (0..count)
            .map(|i| {
                delete(
                    &format!("e{i:05}"),
                    2,
                    DeleteKind::Equality { field_ids: vec![1] },
                )
            })
            .collect();
        (data, deletes)
    }

    #[test]
    fn a_live_delete_is_tested_only_until_it_first_applies() {
        let (data, deletes) = global_deletes(3000);
        let specs = specs();
        let mut pending = PendingDeletes::new(&deletes, &specs);
        assert_eq!(pending.discharge(&data[0]), 3000);
        assert!(pending.is_empty());
        assert_eq!(pending.discharge(&data[1]), 0);
        assert_eq!(pending.into_applied().len(), 3000);
    }

    #[test]
    fn classification_yields_so_a_deadline_can_stop_it() {
        let deletes = (0..3000)
            .map(|i| {
                delete(
                    &format!("e{i:05}"),
                    1,
                    DeleteKind::Equality { field_ids: vec![1] },
                )
            })
            .collect();
        let data = (0..10)
            .map(|i| file(&format!("d{i:05}"), 1, PartitionKey::unpartitioned()))
            .collect();
        let inventory = Inventory::from_entries(data, deletes).unwrap();
        let (specs, floating) = (specs(), BTreeSet::new());
        let classify = dangling(inventory, &specs, &floating, 10);
        // 30,000 tests of deletes that never apply cannot finish in one poll.
        assert!(futures::FutureExt::now_or_never(classify).is_none());
    }

    #[test]
    fn many_live_global_deletes_leave_nothing_dangling() {
        let (data, deletes) = global_deletes(3000);
        let selection = select(data, deletes, 10);
        assert!(selection.deletes.is_empty());
        assert_eq!(selection.examined, 3000);
    }

    #[test]
    fn a_delete_older_than_every_data_file_is_dangling() {
        let data = vec![file("d", 5, PartitionKey::unpartitioned())];
        let deletes = vec![
            delete("p-old", 4, untargeted()),
            delete("p-live", 5, untargeted()),
            delete("e-old", 5, DeleteKind::Equality { field_ids: vec![1] }),
            delete("e-live", 6, DeleteKind::Equality { field_ids: vec![1] }),
        ];
        let selection = select(data, deletes, 10);
        assert_eq!(dangling_paths(&selection), ["e-old", "p-old"]);
        assert_eq!(selection.examined, 4);
        assert_eq!(selection.deferred, 0);
        assert_eq!(selection.deletes[0].content, DeleteContent::Equality);
        assert_eq!(selection.deletes[1].content, DeleteContent::Position);
    }

    #[test]
    fn a_position_delete_whose_referenced_file_is_gone_is_dangling() {
        let data = vec![file("s3://b/kept", 1, PartitionKey::unpartitioned())];
        let deletes = vec![
            delete(
                "p-gone",
                2,
                DeleteKind::Position {
                    referenced: Some("s3://b/compacted".into()),
                    path_bounds: None,
                },
            ),
            delete(
                "p-kept",
                2,
                DeleteKind::Position {
                    referenced: Some("s3://b/kept".into()),
                    path_bounds: None,
                },
            ),
            delete(
                "p-bounds",
                2,
                DeleteKind::Position {
                    referenced: None,
                    path_bounds: Some(("s3://b/a".into(), "s3://b/c".into())),
                },
            ),
        ];
        assert_eq!(
            dangling_paths(&select(data, deletes, 10)),
            ["p-bounds", "p-gone"]
        );
    }

    #[test]
    fn disjoint_key_bounds_prove_an_equality_delete_dangling() {
        let data = vec![bounded(
            file("d", 1, PartitionKey::unpartitioned()),
            1,
            Datum::long(1),
            Datum::long(10),
        )];
        let deletes = vec![
            DeleteEntry {
                file: bounded(
                    file("e-far", 2, PartitionKey::unpartitioned()),
                    1,
                    Datum::long(20),
                    Datum::long(30),
                ),
                kind: DeleteKind::Equality { field_ids: vec![1] },
            },
            DeleteEntry {
                file: bounded(
                    file("e-near", 2, PartitionKey::unpartitioned()),
                    1,
                    Datum::long(5),
                    Datum::long(30),
                ),
                kind: DeleteKind::Equality { field_ids: vec![1] },
            },
            delete("e-unknown", 2, DeleteKind::Equality { field_ids: vec![1] }),
        ];
        assert_eq!(dangling_paths(&select(data, deletes, 10)), ["e-far"]);
    }

    #[test]
    fn floating_point_key_bounds_prove_nothing() {
        let data = vec![bounded(
            file("d", 1, PartitionKey::unpartitioned()),
            2,
            Datum::double(1.0),
            Datum::double(10.0),
        )];
        let deletes = vec![DeleteEntry {
            file: bounded(
                file("e-nan", 2, PartitionKey::unpartitioned()),
                2,
                Datum::double(20.0),
                Datum::double(30.0),
            ),
            kind: DeleteKind::Equality { field_ids: vec![2] },
        }];
        assert!(select(data, deletes, 10).deletes.is_empty());
    }

    #[test]
    fn a_table_without_data_leaves_every_delete_dangling_up_to_the_bound() {
        let deletes = vec![
            delete("a", 1, untargeted()),
            delete("b", 1, untargeted()),
            delete("c", 1, untargeted()),
        ];
        let selection = select(Vec::new(), deletes, 2);
        assert_eq!(dangling_paths(&selection), ["a", "b"]);
        assert_eq!(selection.deferred, 1);
    }

    #[test]
    fn plan_documents_round_trip_with_a_stable_hash() {
        let work = MetadataWork::RemoveDanglingDeletes {
            deletes: vec![PlannedDanglingDelete {
                path: "s3://b/t/data/d.parquet".into(),
                content: DeleteContent::Position,
                size: 10,
                records: 1,
                sequence_number: 3,
            }],
            examined: 4,
            deferred: 0,
        };
        let document = MetadataPlanDocument::new(
            JobId::generate(),
            PlanId::generate(),
            PlanningBaseline::for_tests(),
            work,
        )
        .unwrap();
        let value = document.to_value().unwrap();
        let back = MetadataPlanDocument::from_persisted(value).unwrap();
        assert_eq!(back, document);
        assert_eq!(
            back.content_hash().unwrap(),
            document.content_hash().unwrap()
        );
    }

    #[test]
    fn empty_or_duplicated_work_is_rejected() {
        let baseline = PlanningBaseline::for_tests();
        let empty = MetadataWork::RewriteManifests {
            target_size_bytes: 1,
            manifests: Vec::new(),
            deferred: 0,
        };
        assert!(
            MetadataPlanDocument::new(
                JobId::generate(),
                PlanId::generate(),
                baseline.clone(),
                empty
            )
            .is_err()
        );
        let twice = PlannedManifest {
            path: "m".into(),
            length: 1,
            spec_id: 0,
            live_files: 1,
        };
        let duplicated = MetadataWork::RewriteManifests {
            target_size_bytes: 1,
            manifests: vec![twice.clone(), twice],
            deferred: 0,
        };
        assert!(
            MetadataPlanDocument::new(JobId::generate(), PlanId::generate(), baseline, duplicated)
                .is_err()
        );
    }
}
