//! Commit building against real Iceberg metadata and manifests in memory
//! storage. Each "publication" applies the captured updates to the metadata,
//! as a catalog would.

use bergsmith_common::ids::{JobId, PublicationId};
use bergsmith_storage::{Location, ObjectStore, StorageProfile};
use iceberg::spec::{
    DataContentType, DataFile, DataFileBuilder, DataFileFormat, ManifestStatus, NestedField,
    PrimitiveType, Schema, Struct, TableMetadata, TableMetadataBuilder, Type,
};
use iceberg::transaction::ApplyTransactionAction;
use iceberg::{TableCreation, TableRequirement};

use super::*;
use crate::connection::{LoadedTable, TableName};

const LOCATION: &str = "memory://warehouse/db/t";

fn store() -> ObjectStore {
    ObjectStore::open(&StorageProfile::Memory, None).unwrap()
}

fn name() -> TableName {
    TableName {
        namespace: vec!["db".to_owned()],
        name: "t".to_owned(),
    }
}

fn empty_table() -> TableMetadata {
    let schema = Schema::builder()
        .with_fields([NestedField::required(1, "id", Type::Primitive(PrimitiveType::Long)).into()])
        .build()
        .unwrap();
    let creation = TableCreation::builder()
        .name("t".to_owned())
        .location(LOCATION.to_owned())
        .schema(schema)
        .build();
    TableMetadataBuilder::from_table_creation(creation)
        .unwrap()
        .build()
        .unwrap()
        .metadata
}

fn baseline(store: &ObjectStore, metadata: &TableMetadata) -> Baseline {
    let loaded = LoadedTable {
        name: name(),
        metadata_location: Some(format!("{LOCATION}/metadata/v1.metadata.json")),
        metadata: metadata.clone(),
        location: Location::parse(LOCATION).unwrap(),
        credentials: Vec::new(),
    };
    Baseline::new(&loaded, store.file_io().clone()).unwrap()
}

fn data_file(name: &str, records: u64) -> DataFile {
    DataFileBuilder::default()
        .content(DataContentType::Data)
        .file_path(format!("{LOCATION}/data/{name}.parquet"))
        .file_format(DataFileFormat::Parquet)
        .file_size_in_bytes(records * 10)
        .record_count(records)
        .partition_spec_id(0)
        .partition(Struct::empty())
        .build()
        .unwrap()
}

/// Applies a request's updates, checking its requirements first, as a
/// catalog would.
fn publish(metadata: &TableMetadata, request: &CommitRequest) -> TableMetadata {
    let decoded = request.decode().unwrap();
    for requirement in &decoded.requirements {
        requirement.check(Some(metadata)).unwrap();
    }
    let mut builder = metadata.clone().into_builder(None);
    for update in decoded.updates {
        builder = update.apply(builder).unwrap();
    }
    builder.build().unwrap().metadata
}

async fn append(
    store: &ObjectStore,
    metadata: &TableMetadata,
    files: &[DataFile],
) -> TableMetadata {
    let request = baseline(store, metadata)
        .prepare_with(|transaction| {
            transaction
                .fast_append()
                .add_data_files(files.to_vec())
                .apply(transaction)
        })
        .await
        .unwrap();
    publish(metadata, &request)
}

fn proposal(
    metadata: &TableMetadata,
    removed: Vec<DataFile>,
    added: Vec<DataFile>,
) -> RewriteProposal {
    RewriteProposal {
        job_id: JobId::generate(),
        publication_id: PublicationId::generate(),
        snapshot_id: new_snapshot_id(metadata),
        removed,
        added,
    }
}

/// An empty table plus one append of `a` and `b`.
async fn appended(store: &ObjectStore) -> TableMetadata {
    append(
        store,
        &empty_table(),
        &[data_file("a", 1), data_file("b", 2)],
    )
    .await
}

#[tokio::test]
async fn baselines_assert_every_standard_requirement() {
    let store = store();
    let metadata = appended(&store).await;
    let main = metadata.current_snapshot_id();

    let requirements = baseline(&store, &metadata).requirements().to_vec();

    assert_eq!(
        requirements,
        [
            TableRequirement::UuidMatch {
                uuid: metadata.uuid()
            },
            TableRequirement::RefSnapshotIdMatch {
                r#ref: "main".to_owned(),
                snapshot_id: main,
            },
            TableRequirement::CurrentSchemaIdMatch {
                current_schema_id: 0
            },
            TableRequirement::LastAssignedFieldIdMatch {
                last_assigned_field_id: 1
            },
            TableRequirement::DefaultSpecIdMatch { default_spec_id: 0 },
            TableRequirement::LastAssignedPartitionIdMatch {
                last_assigned_partition_id: metadata.last_partition_id()
            },
            TableRequirement::DefaultSortOrderIdMatch {
                default_sort_order_id: 0
            },
        ]
    );
    assert!(main.is_some());
}

#[tokio::test]
async fn an_empty_table_asserts_that_main_does_not_exist() {
    let store = store();
    let empty = empty_table();

    let request = baseline(&store, &empty)
        .prepare_with(|transaction| {
            transaction
                .fast_append()
                .add_data_files([data_file("a", 1)])
                .apply(transaction)
        })
        .await
        .unwrap();

    assert!(request.decode().unwrap().requirements.contains(
        &TableRequirement::RefSnapshotIdMatch {
            r#ref: "main".to_owned(),
            snapshot_id: None,
        }
    ));
}

#[tokio::test]
async fn a_rewrite_replaces_files_in_an_explicit_snapshot() {
    let store = store();
    let metadata = appended(&store).await;
    let main = metadata.current_snapshot().unwrap().clone();
    let baseline = baseline(&store, &metadata);
    let proposal = proposal(
        &metadata,
        vec![data_file("a", 1), data_file("b", 2)],
        vec![data_file("c", 3)],
    );
    let (snapshot_id, job_id, publication_id) = (
        proposal.snapshot_id,
        proposal.job_id,
        proposal.publication_id,
    );
    let hash = proposal.proposal_hash(&baseline).unwrap();

    let prepared = proposal.prepare(&baseline).await.unwrap();
    let published = publish(&metadata, &prepared.request);

    let snapshot = published.snapshot_by_id(snapshot_id).unwrap();
    assert_eq!(published.current_snapshot_id(), Some(snapshot_id));
    assert_eq!(snapshot.parent_snapshot_id(), Some(main.snapshot_id()));
    let summary = &snapshot.summary().additional_properties;
    assert_eq!(summary[JOB_ID_PROPERTY], job_id.to_string());
    assert_eq!(summary[PUBLICATION_ID_PROPERTY], publication_id.to_string());
    assert_eq!(summary[PROPOSAL_HASH_PROPERTY], hash.to_hex());
    assert_eq!(
        prepared.request.decode().unwrap().requirements,
        baseline.requirements()
    );
    assert_eq!(prepared.expected.proposal_hash, hash);
    assert_eq!(prepared.expected.manifest_list, snapshot.manifest_list());

    // Live files after the rewrite: only `c`, at the baseline's data
    // sequence number rather than the new snapshot's.
    let published_baseline = self::baseline(&store, &published);
    let table = published_baseline.table();
    let manifests = table.manifest_list_reader(snapshot).load().await.unwrap();
    let mut live = Vec::new();
    for manifest in manifests.entries() {
        for entry in manifest
            .load_manifest(table.file_io())
            .await
            .unwrap()
            .entries()
        {
            if entry.status() != ManifestStatus::Deleted {
                live.push((
                    entry.data_file().file_path().to_owned(),
                    entry.sequence_number(),
                ));
            }
        }
    }
    assert_eq!(
        live,
        [(
            format!("{LOCATION}/data/c.parquet"),
            Some(main.sequence_number())
        )]
    );
    assert!(snapshot.sequence_number() > main.sequence_number());
}

#[tokio::test]
async fn requirements_stay_on_the_baseline_after_the_table_advances() {
    let store = store();
    let metadata = appended(&store).await;
    let baseline = baseline(&store, &metadata);
    let advanced = append(&store, &metadata, &[data_file("late", 1)]).await;

    let prepared = proposal(&metadata, vec![data_file("a", 1)], vec![data_file("c", 1)])
        .prepare(&baseline)
        .await
        .unwrap();
    let requirements = prepared.request.decode().unwrap().requirements;

    assert_eq!(requirements, baseline.requirements());
    let stale = requirements
        .iter()
        .find(|requirement| requirement.check(Some(&advanced)).is_err());
    assert!(
        matches!(stale, Some(TableRequirement::RefSnapshotIdMatch { snapshot_id, .. })
            if *snapshot_id == metadata.current_snapshot_id()),
        "{stale:?}"
    );
}

#[tokio::test]
async fn removing_a_file_that_is_not_live_fails() {
    let store = store();
    let metadata = appended(&store).await;

    let error = proposal(&metadata, vec![data_file("missing", 1)], Vec::new())
        .prepare(&baseline(&store, &metadata))
        .await
        .unwrap_err();

    assert!(matches!(error, ProposalError::Iceberg(_)), "{error}");
}

#[tokio::test]
async fn invalid_proposals_are_rejected_before_building() {
    let store = store();
    let metadata = appended(&store).await;
    let empty = empty_table();
    let existing = metadata.current_snapshot_id().unwrap();

    let nothing_removed = proposal(&metadata, Vec::new(), vec![data_file("c", 1)])
        .prepare(&baseline(&store, &metadata))
        .await
        .unwrap_err();
    let mut reused = proposal(&metadata, vec![data_file("a", 1)], Vec::new());
    reused.snapshot_id = existing;
    let reused = reused
        .prepare(&baseline(&store, &metadata))
        .await
        .unwrap_err();
    let no_main = proposal(&empty, vec![data_file("a", 1)], Vec::new())
        .prepare(&baseline(&store, &empty))
        .await
        .unwrap_err();

    assert!(matches!(nothing_removed, ProposalError::NothingRemoved));
    assert!(matches!(reused, ProposalError::SnapshotIdInUse(id) if id == existing));
    assert!(matches!(no_main, ProposalError::NoMainSnapshot));
}

#[tokio::test]
async fn a_rewrite_with_no_outputs_removes_the_files() {
    let store = store();
    let metadata = appended(&store).await;
    let prepared = proposal(&metadata, vec![data_file("a", 1)], Vec::new())
        .prepare(&baseline(&store, &metadata))
        .await
        .unwrap();

    let published = publish(&metadata, &prepared.request);

    assert_eq!(
        published.current_snapshot_id(),
        Some(prepared.expected.snapshot_id)
    );
}

#[tokio::test]
async fn proposal_hashes_ignore_file_order_and_cover_every_field() {
    let store = store();
    let metadata = appended(&store).await;
    let baseline = baseline(&store, &metadata);
    let original = proposal(
        &metadata,
        vec![data_file("a", 1), data_file("b", 2)],
        vec![data_file("c", 3)],
    );
    let hash = |p: &RewriteProposal| p.proposal_hash(&baseline).unwrap();

    let mut reordered = original.clone();
    reordered.removed.reverse();
    assert_eq!(hash(&reordered), hash(&original));

    let mut variants = Vec::new();
    let mut changed = original.clone();
    changed.job_id = JobId::generate();
    variants.push(changed);
    let mut changed = original.clone();
    changed.publication_id = PublicationId::generate();
    variants.push(changed);
    let mut changed = original.clone();
    changed.snapshot_id += 1;
    variants.push(changed);
    let mut changed = original.clone();
    changed.added = vec![data_file("c", 4)];
    variants.push(changed);
    let mut changed = original.clone();
    changed.removed.pop();
    variants.push(changed);
    let mut changed = original.clone();
    std::mem::swap(&mut changed.removed, &mut changed.added);
    variants.push(changed);
    for variant in &variants {
        assert_ne!(hash(variant), hash(&original), "{variant:?}");
    }

    let advanced = append(&store, &metadata, &[data_file("late", 1)]).await;
    let other_baseline = self::baseline(&store, &advanced);
    assert_ne!(
        original.proposal_hash(&other_baseline).unwrap(),
        hash(&original)
    );
    assert!(matches!(
        original.proposal_hash(&self::baseline(&store, &empty_table())),
        Err(ProposalError::NoMainSnapshot)
    ));
}

#[tokio::test]
async fn evidence_recognizes_only_the_exact_published_snapshot() {
    let store = store();
    let metadata = appended(&store).await;
    let prepared = proposal(&metadata, vec![data_file("a", 1)], vec![data_file("c", 1)])
        .prepare(&baseline(&store, &metadata))
        .await
        .unwrap();
    let expected = prepared.expected.clone();
    let published = publish(&metadata, &prepared.request);
    let sequence_number = published
        .snapshot_by_id(expected.snapshot_id)
        .unwrap()
        .sequence_number();
    let advanced = append(&store, &published, &[data_file("late", 1)]).await;

    assert_eq!(expected.find_in(&metadata), SnapshotEvidence::Absent);
    assert_eq!(
        expected.find_in(&published),
        SnapshotEvidence::Published {
            sequence_number,
            main_head: true
        }
    );
    assert_eq!(
        expected.find_in(&advanced),
        SnapshotEvidence::Published {
            sequence_number,
            main_head: false
        }
    );

    let mismatch = |change: fn(&mut ExpectedSnapshot)| {
        let mut other = expected.clone();
        change(&mut other);
        other.find_in(&published)
    };
    assert_eq!(
        mismatch(|e| e.table_uuid = uuid::Uuid::nil()),
        SnapshotEvidence::Mismatch(EvidenceMismatch::TableUuid(published.uuid()))
    );
    assert_eq!(
        mismatch(|e| e.parent_snapshot_id = None),
        SnapshotEvidence::Mismatch(EvidenceMismatch::ParentSnapshot(
            metadata.current_snapshot_id()
        ))
    );
    assert!(matches!(
        mismatch(|e| e.manifest_list = "other".to_owned()),
        SnapshotEvidence::Mismatch(EvidenceMismatch::ManifestList(_))
    ));
    assert!(matches!(
        mismatch(|e| e.job_id = JobId::generate()),
        SnapshotEvidence::Mismatch(EvidenceMismatch::SummaryProperty {
            key: JOB_ID_PROPERTY,
            found: Some(_)
        })
    ));
    assert!(matches!(
        mismatch(|e| e.proposal_hash = bergsmith_common::digest::Sha256Digest::of(b"x", b"")),
        SnapshotEvidence::Mismatch(EvidenceMismatch::SummaryProperty {
            key: PROPOSAL_HASH_PROPERTY,
            ..
        })
    ));
}

#[tokio::test]
async fn actions_that_change_nothing_are_rejected() {
    let store = store();
    let metadata = appended(&store).await;

    let error = baseline(&store, &metadata)
        .prepare_with(Ok)
        .await
        .unwrap_err();

    assert!(matches!(error, ProposalError::NoChanges), "{error}");
}

fn loaded(metadata: &TableMetadata, metadata_location: &str) -> LoadedTable {
    LoadedTable {
        name: name(),
        metadata_location: Some(metadata_location.to_owned()),
        metadata: metadata.clone(),
        location: Location::parse(LOCATION).unwrap(),
        credentials: Vec::new(),
    }
}

fn apply_json(metadata: &TableMetadata, updates: serde_json::Value) -> TableMetadata {
    let updates: Vec<iceberg::TableUpdate> = serde_json::from_value(updates).unwrap();
    let mut builder = metadata.clone().into_builder(None);
    for update in updates {
        builder = update.apply(builder).unwrap();
    }
    builder.build().unwrap().metadata
}

#[tokio::test]
async fn an_unchanged_reload_shows_no_drift() {
    let store = store();
    let metadata = appended(&store).await;

    let changes = baseline(&store, &metadata).changes(&loaded(
        &metadata,
        &format!("{LOCATION}/metadata/v1.metadata.json"),
    ));

    assert_eq!(changes, []);
}

#[tokio::test]
async fn every_observed_baseline_change_is_reported() {
    let store = store();
    let metadata = appended(&store).await;
    let main = metadata.current_snapshot_id();
    let advanced = append(&store, &metadata, &[data_file("late", 1)]).await;
    let reshaped = apply_json(
        &advanced,
        serde_json::json!([
            {"action": "add-schema", "schema": {"type": "struct", "schema-id": 1, "fields": [
                {"id": 1, "name": "id", "required": true, "type": "long"},
                {"id": 2, "name": "extra", "required": false, "type": "string"}]}},
            {"action": "set-current-schema", "schema-id": -1},
            {"action": "add-spec", "spec": {"fields": [
                {"source-id": 1, "field-id": 1000, "name": "id_bucket", "transform": "bucket[4]"}]}},
            {"action": "set-default-spec", "spec-id": -1},
            {"action": "add-sort-order", "sort-order": {"order-id": 1, "fields": [
                {"source-id": 1, "transform": "identity", "direction": "asc",
                 "null-order": "nulls-first"}]}},
            {"action": "set-default-sort-order", "sort-order-id": -1},
            {"action": "set-properties", "updates": {
                "write.data.path": format!("{LOCATION}/elsewhere"), "owner": "someone"}}
        ]),
    );

    let changes = baseline(&store, &metadata).changes(&loaded(
        &reshaped,
        &format!("{LOCATION}/metadata/v2.metadata.json"),
    ));

    assert_eq!(
        changes,
        [
            BaselineChange::MetadataLocation,
            BaselineChange::MainSnapshot {
                from: main,
                to: advanced.current_snapshot_id()
            },
            BaselineChange::CurrentSchema,
            BaselineChange::LastColumnId,
            BaselineChange::DefaultSpec,
            BaselineChange::LastPartitionId,
            BaselineChange::DefaultSortOrder,
            BaselineChange::Property("write.data.path"),
        ]
    );
}

#[tokio::test]
async fn a_replaced_table_and_format_upgrade_are_reported() {
    let store = store();
    let metadata = empty_table();
    let location = format!("{LOCATION}/metadata/v1.metadata.json");
    let upgraded = apply_json(
        &metadata,
        serde_json::json!([{"action": "upgrade-format-version", "format-version": 3}]),
    );
    let replaced = empty_table();

    let baseline = baseline(&store, &metadata);

    assert_eq!(
        baseline.changes(&loaded(&upgraded, &location)),
        [BaselineChange::FormatVersion]
    );
    assert_eq!(
        baseline.changes(&loaded(&replaced, &location)),
        [BaselineChange::TableUuid]
    );
}

#[test]
fn data_files_go_under_the_table_unless_write_data_path_says_otherwise() {
    let metadata = empty_table();
    let custom = apply_json(
        &metadata,
        serde_json::json!([{"action": "set-properties",
            "updates": {"write.data.path": format!("{LOCATION}/custom/")}}]),
    );
    let outside = apply_json(
        &metadata,
        serde_json::json!([{"action": "set-properties",
            "updates": {"write.data.path": "memory://other/data"}}]),
    );
    let invalid = apply_json(
        &metadata,
        serde_json::json!([{"action": "set-properties",
            "updates": {"write.data.path": "no-scheme"}}]),
    );
    let location = |metadata: &TableMetadata| loaded(metadata, "m").data_location();

    assert_eq!(
        location(&metadata).unwrap(),
        Location::parse(&format!("{LOCATION}/data")).unwrap()
    );
    assert_eq!(
        location(&custom).unwrap(),
        Location::parse(&format!("{LOCATION}/custom")).unwrap()
    );
    assert!(matches!(
        location(&outside),
        Err(crate::CatalogError::DataLocationOutsideTable(_))
    ));
    assert!(matches!(
        location(&invalid),
        Err(crate::CatalogError::InvalidLocation(_))
    ));
}

#[tokio::test]
async fn a_literal_update_commit_asserts_exactly_the_baseline_requirements() {
    let store = store();
    let metadata = empty_table();
    let baseline = baseline(&store, &metadata);
    let snapshot_ids = vec![7_i64, 9];

    let request = baseline
        .commit_with_updates(vec![iceberg::TableUpdate::RemoveSnapshots {
            snapshot_ids: snapshot_ids.clone(),
        }])
        .unwrap();
    let decoded = request.decode().unwrap();

    assert_eq!(
        decoded.requirements,
        baseline.requirements().to_vec(),
        "a literal-update commit must assert the baseline requirements"
    );
    assert!(
        decoded.updates.iter().any(|update| matches!(
            update,
            iceberg::TableUpdate::RemoveSnapshots { snapshot_ids: ids } if ids == &snapshot_ids
        )),
        "the request must carry the exact snapshot ids"
    );
}

/// An empty table plus three appends, one manifest each.
async fn fragmented(store: &ObjectStore) -> TableMetadata {
    let mut metadata = empty_table();
    for name in ["a", "b", "c"] {
        metadata = append(store, &metadata, &[data_file(name, 1)]).await;
    }
    metadata
}

async fn data_manifests(store: &ObjectStore, metadata: &TableMetadata) -> Vec<String> {
    let baseline = baseline(store, metadata);
    let table = baseline.table();
    let snapshot = metadata.current_snapshot().unwrap();
    let list = table.manifest_list_reader(snapshot).load().await.unwrap();
    list.entries()
        .iter()
        .map(|manifest| manifest.manifest_path.clone())
        .collect()
}

async fn live_paths(store: &ObjectStore, metadata: &TableMetadata) -> Vec<String> {
    let baseline = baseline(store, metadata);
    let table = baseline.table();
    let snapshot = metadata.current_snapshot().unwrap();
    let list = table.manifest_list_reader(snapshot).load().await.unwrap();
    let mut live = Vec::new();
    for manifest in list.entries() {
        for entry in manifest
            .load_manifest(table.file_io())
            .await
            .unwrap()
            .entries()
        {
            if entry.is_alive() {
                live.push(entry.data_file().file_path().to_owned());
            }
        }
    }
    live.sort();
    live
}

fn manifest_proposal(metadata: &TableMetadata, manifests: Vec<String>) -> ManifestRewriteProposal {
    ManifestRewriteProposal {
        job_id: JobId::generate(),
        publication_id: PublicationId::generate(),
        snapshot_id: new_snapshot_id(metadata),
        manifests,
        target_size_bytes: 8 * 1024 * 1024,
    }
}

#[tokio::test]
async fn a_manifest_rewrite_merges_small_manifests_and_keeps_every_file() {
    let store = store();
    let metadata = fragmented(&store).await;
    let main = metadata.current_snapshot().unwrap().clone();
    let manifests = data_manifests(&store, &metadata).await;
    assert_eq!(manifests.len(), 3);
    let before = live_paths(&store, &metadata).await;
    let baseline = baseline(&store, &metadata);
    let proposal = manifest_proposal(&metadata, manifests.clone());
    let (snapshot_id, job_id, publication_id) = (
        proposal.snapshot_id,
        proposal.job_id,
        proposal.publication_id,
    );
    let hash = proposal.proposal_hash(&baseline).unwrap();

    let prepared = proposal.prepare(&baseline).await.unwrap();
    let published = publish(&metadata, &prepared.request);

    let snapshot = published.snapshot_by_id(snapshot_id).unwrap();
    assert_eq!(published.current_snapshot_id(), Some(snapshot_id));
    assert_eq!(snapshot.parent_snapshot_id(), Some(main.snapshot_id()));
    assert_eq!(
        snapshot.summary().operation,
        iceberg::spec::Operation::Replace
    );
    let summary = &snapshot.summary().additional_properties;
    assert_eq!(summary[JOB_ID_PROPERTY], job_id.to_string());
    assert_eq!(summary[PUBLICATION_ID_PROPERTY], publication_id.to_string());
    assert_eq!(summary[PROPOSAL_HASH_PROPERTY], hash.to_hex());
    assert_eq!(
        prepared.request.decode().unwrap().requirements,
        baseline.requirements()
    );
    assert_eq!(prepared.expected.manifest_list, snapshot.manifest_list());
    let after = data_manifests(&store, &published).await;
    assert_eq!(after.len(), 1);
    assert!(after[0].contains(&publication_id.to_string()), "{after:?}");
    assert_eq!(live_paths(&store, &published).await, before);
}

#[tokio::test]
async fn a_manifest_rewrite_rewrites_only_the_planned_manifests() {
    let store = store();
    let metadata = fragmented(&store).await;
    let manifests = data_manifests(&store, &metadata).await;
    let baseline = baseline(&store, &metadata);
    let planned = vec![manifests[0].clone(), manifests[1].clone()];

    let prepared = manifest_proposal(&metadata, planned.clone())
        .prepare(&baseline)
        .await
        .unwrap();
    let published = publish(&metadata, &prepared.request);

    let after = data_manifests(&store, &published).await;
    assert_eq!(after.len(), 2);
    assert!(after.contains(&manifests[2]));
    assert!(!after.iter().any(|path| planned.contains(path)));
    assert_eq!(live_paths(&store, &published).await.len(), 3);
}

#[tokio::test]
async fn invalid_manifest_rewrites_are_rejected_before_building() {
    let store = store();
    let metadata = fragmented(&store).await;
    let manifests = data_manifests(&store, &metadata).await;
    let baseline = baseline(&store, &metadata);

    for planned in [
        vec![manifests[0].clone()],
        vec![manifests[0].clone(), manifests[0].clone()],
        vec![
            manifests[0].clone(),
            format!("{LOCATION}/metadata/missing.avro"),
        ],
    ] {
        let error = manifest_proposal(&metadata, planned)
            .prepare(&baseline)
            .await
            .unwrap_err();
        assert!(matches!(error, ProposalError::NothingRemoved), "{error}");
    }
    let mut taken = manifest_proposal(&metadata, manifests.clone());
    taken.snapshot_id = metadata.current_snapshot_id().unwrap();
    assert!(matches!(
        taken.prepare(&baseline).await,
        Err(ProposalError::SnapshotIdInUse(_))
    ));
    let empty = empty_table();
    assert!(matches!(
        manifest_proposal(&empty, manifests)
            .prepare(&self::baseline(&store, &empty))
            .await,
        Err(ProposalError::NoMainSnapshot)
    ));
}

#[tokio::test]
async fn manifest_proposal_hashes_ignore_order_and_cover_the_target_size() {
    let store = store();
    let metadata = fragmented(&store).await;
    let manifests = data_manifests(&store, &metadata).await;
    let baseline = baseline(&store, &metadata);
    let proposal = manifest_proposal(&metadata, manifests.clone());
    let hash = proposal.proposal_hash(&baseline).unwrap();

    let mut reversed = proposal.clone();
    reversed.manifests.reverse();
    assert_eq!(reversed.proposal_hash(&baseline).unwrap(), hash);
    let mut resized = proposal.clone();
    resized.target_size_bytes += 1;
    assert_ne!(resized.proposal_hash(&baseline).unwrap(), hash);
    let mut fewer = proposal;
    fewer.manifests.pop();
    assert_ne!(fewer.proposal_hash(&baseline).unwrap(), hash);
}
