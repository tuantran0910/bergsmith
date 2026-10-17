//! BS-011.T2–T3: execute real Parquet, prepare through the capture catalog,
//! and inspect the resulting Avro manifests and independent live-row multiset.
//! Applying captured updates here models publication; it does not certify a
//! distributed result-acceptance or recovery path (BS-012).

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "acceptance fixtures fail the test on setup errors"
)]

use std::collections::{BTreeMap, BTreeSet};

use bergsmith_catalog::{
    Baseline, CommitRequest, LoadedTable, RewriteProposal, TableName, new_snapshot_id,
};
use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{AttemptId, JobId, PlanId, PublicationId};
use bergsmith_fixtures::{Fixture, live_rows, read_data_files, spike};
use bergsmith_maintenance::{
    AttemptContext, ExecutionInput, ExecutionLimits, ExecutionRequest, Executor, NativeExecutor,
    PlanIds, PlanInput, PlanOutcome, Planner, PlannerConfig, PlanningBaseline,
};
use bergsmith_protocol::hashing::artifact_digest;
use bergsmith_storage::Location;
use iceberg::io::FileIO;
use iceberg::spec::{
    DataContentType, DataFile, FormatVersion, ManifestList, ManifestStatus, TableMetadata,
};

fn baseline(fixture: &Fixture, metadata: &TableMetadata) -> Baseline {
    Baseline::new(
        &LoadedTable {
            name: TableName {
                namespace: vec!["test".into()],
                name: "events".into(),
            },
            metadata_location: Some(fixture.metadata_location().into()),
            metadata: metadata.clone(),
            location: Location::parse(metadata.location()).unwrap(),
            credentials: vec![],
        },
        fixture.file_io().clone(),
    )
    .unwrap()
}

fn publish(metadata: &TableMetadata, request: &CommitRequest) -> TableMetadata {
    let request = request.decode().unwrap();
    for requirement in request.requirements {
        requirement.check(Some(metadata)).unwrap();
    }
    let mut builder = metadata.clone().into_builder(None);
    for update in request.updates {
        builder = update.apply(builder).unwrap();
    }
    builder.build().unwrap().metadata
}

#[derive(Debug)]
struct Entry {
    file: DataFile,
    status: ManifestStatus,
    sequence: i64,
    file_sequence: i64,
}

async fn entries(metadata: &TableMetadata, io: &FileIO) -> Vec<Entry> {
    let snapshot = metadata.current_snapshot().unwrap();
    let bytes = io
        .new_input(snapshot.manifest_list())
        .unwrap()
        .read()
        .await
        .unwrap();
    let list = ManifestList::parse_with_version(&bytes, FormatVersion::V2).unwrap();
    let mut entries = Vec::new();
    for manifest in list.entries() {
        let loaded = manifest.load_manifest(io).await.unwrap();
        for entry in loaded.entries() {
            entries.push(Entry {
                file: entry.data_file().clone(),
                status: entry.status(),
                sequence: entry.sequence_number().unwrap(),
                file_sequence: entry.file_sequence_number.unwrap(),
            });
        }
    }
    entries
}

async fn hashes(fixture: &Fixture) -> BTreeMap<String, Sha256Digest> {
    let mut hashes = BTreeMap::new();
    for entry in entries(fixture.metadata(), fixture.file_io()).await {
        let path = entry.file.file_path();
        let bytes = fixture
            .file_io()
            .new_input(path)
            .unwrap()
            .read()
            .await
            .unwrap();
        hashes.insert(
            path.into(),
            Sha256Digest::of(b"source-preservation", &bytes),
        );
    }
    hashes
}

struct Rewritten {
    metadata: TableMetadata,
    outputs: Vec<DataFile>,
    selected: BTreeSet<String>,
}

async fn rewrite(fixture: &Fixture, selection: Option<&[&str]>) -> Rewritten {
    let metadata = fixture.metadata();
    let original = entries(metadata, fixture.file_io()).await;
    let before = hashes(fixture).await;
    let selected: BTreeSet<String> = match selection {
        Some(names) => names
            .iter()
            .map(|name| fixture.path_of(name).unwrap().into())
            .collect(),
        None => original
            .iter()
            .filter(|e| e.file.content_type() == DataContentType::Data)
            .map(|e| e.file.file_path().into())
            .collect(),
    };
    let expected = live_rows(metadata, fixture.file_io(), Some(&selected))
        .await
        .unwrap();
    let (job_id, outputs) = execute_selected(fixture, metadata, &selected).await;
    assert_eq!(
        read_data_files(metadata, fixture.file_io(), &outputs)
            .await
            .unwrap(),
        expected
    );
    let pinned = baseline(fixture, metadata);
    let proposal = RewriteProposal {
        job_id,
        publication_id: PublicationId::generate(),
        snapshot_id: new_snapshot_id(metadata),
        removed: original
            .iter()
            .filter(|e| selected.contains(e.file.file_path()))
            .map(|e| e.file.clone())
            .collect(),
        added: outputs.clone(),
    }
    .prepare(&pinned)
    .await
    .unwrap();
    let published = publish(metadata, &proposal.request);
    assert_eq!(
        live_rows(&published, fixture.file_io(), None)
            .await
            .unwrap(),
        fixture.live_rows().await.unwrap()
    );
    for (path, hash) in before {
        let bytes = fixture
            .file_io()
            .new_input(&path)
            .unwrap()
            .read()
            .await
            .unwrap();
        assert_eq!(
            Sha256Digest::of(b"source-preservation", &bytes),
            hash,
            "{path}"
        );
    }
    Rewritten {
        metadata: published,
        outputs,
        selected,
    }
}

async fn execute_selected(
    fixture: &Fixture,
    metadata: &TableMetadata,
    selected: &BTreeSet<String>,
) -> (JobId, Vec<DataFile>) {
    let job_id = JobId::generate();
    let outcome = Planner::new(PlannerConfig {
        min_input_files: 1,
        min_files_removed: 0,
        ..PlannerConfig::default()
    })
    .unwrap()
    .plan(
        PlanInput {
            metadata,
            metadata_location: Some(fixture.metadata_location()),
            file_io: fixture.file_io(),
            selection: &[],
        },
        PlanIds {
            job_id,
            plan_id: PlanId::generate(),
        },
    )
    .await
    .unwrap();
    let PlanOutcome::Planned(plan) = outcome else {
        panic!("expected planned rewrite: {outcome:?}");
    };
    let planning = PlanningBaseline::capture(metadata, Some(fixture.metadata_location()))
        .unwrap()
        .unwrap();
    let mut outputs = Vec::new();
    let mut executed = BTreeSet::new();
    for task in plan.tasks() {
        let mut artifact = plan.artifact_of(task).unwrap();
        artifact
            .source_files
            .retain(|source| selected.contains(&source.path));
        if artifact.source_files.is_empty() {
            continue;
        }
        executed.extend(
            artifact
                .source_files
                .iter()
                .map(|source| source.path.clone()),
        );
        let attempt_id = AttemptId::generate();
        let prefix = Location::parse(planning.data_location())
            .unwrap()
            .attempt_output_prefix(job_id, attempt_id);
        let result = NativeExecutor
            .execute(ExecutionRequest {
                input: ExecutionInput {
                    artifact: &artifact,
                    artifact_sha256: artifact_digest(&artifact),
                    metadata,
                    metadata_location: fixture.metadata_location(),
                    file_io: fixture.file_io(),
                },
                attempt: AttemptContext {
                    attempt_id,
                    generation: 1,
                },
                output_prefix: &prefix,
                limits: ExecutionLimits::default(),
            })
            .await
            .unwrap();
        assert_eq!(result.receipt().outputs.len(), result.data_files().len());
        let metrics = result.receipt().metrics.as_ref().unwrap();
        if result.data_files().is_empty() {
            assert!(result.receipt().outputs.is_empty());
            assert_eq!(metrics.rows_written, 0);
            assert_eq!(metrics.rows_deleted, metrics.rows_read);
            assert_eq!(metrics.bytes_written, 0);
        }
        outputs.extend_from_slice(result.data_files());
    }
    assert_eq!(&executed, selected);
    (job_id, outputs)
}

async fn assert_manifests(fixture: &Fixture, rewritten: &Rewritten) {
    let original = entries(fixture.metadata(), fixture.file_io()).await;
    let expected_paths: BTreeSet<_> = original
        .iter()
        .filter(|e| !rewritten.selected.contains(e.file.file_path()))
        .map(|e| e.file.file_path())
        .chain(rewritten.outputs.iter().map(DataFile::file_path))
        .collect();
    let after = entries(&rewritten.metadata, fixture.file_io()).await;
    let start = fixture
        .metadata()
        .current_snapshot()
        .unwrap()
        .sequence_number();
    let committed = rewritten
        .metadata
        .current_snapshot()
        .unwrap()
        .sequence_number();
    assert_eq!(committed, start + 1);
    let mut live_paths = BTreeSet::new();
    for entry in after.iter().filter(|e| e.status != ManifestStatus::Deleted) {
        assert!(live_paths.insert(entry.file.file_path()));
    }
    for original in &original {
        let path = original.file.file_path();
        if rewritten.selected.contains(path) {
            assert!(
                !live_paths.contains(path),
                "selected source remains live: {path}"
            );
        } else {
            let retained = after
                .iter()
                .find(|e| e.status != ManifestStatus::Deleted && e.file.file_path() == path)
                .unwrap();
            assert_eq!(retained.file, original.file);
            assert_eq!(retained.sequence, original.sequence);
            assert_eq!(retained.file_sequence, original.file_sequence);
        }
    }
    for output in &rewritten.outputs {
        let entry = after
            .iter()
            .find(|e| e.file.file_path() == output.file_path())
            .unwrap();
        assert_eq!(entry.status, ManifestStatus::Added);
        assert_eq!(entry.sequence, start);
        assert_eq!(entry.file_sequence, committed);
        assert_eq!(&entry.file, output);
    }
    assert_eq!(live_paths, expected_paths);
}

#[tokio::test]
async fn replacements_use_the_planning_sequence_and_keep_shared_deletes_live() {
    let fixture = spike::shared_deletes_with_an_untouched_file()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let rewritten = rewrite(&fixture, Some(&["a", "b"])).await;
    assert!(!rewritten.outputs.is_empty());
    assert_manifests(&fixture, &rewritten).await;
}

#[tokio::test]
async fn same_sequence_survivors_are_not_deleted_again_after_publication() {
    let fixture = spike::equality_deletes_with_old_and_same_sequence_data()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let rewritten = rewrite(&fixture, None).await;
    assert_manifests(&fixture, &rewritten).await;
    assert_eq!(
        live_rows(&rewritten.metadata, fixture.file_io(), None)
            .await
            .unwrap()
            .len(),
        6
    );
}

#[tokio::test]
async fn zero_output_receipt_publishes_source_removal_and_retains_deletes() {
    let fixture = spike::fully_deleted_group()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let rewritten = rewrite(&fixture, None).await;
    assert!(rewritten.outputs.is_empty());
    assert_manifests(&fixture, &rewritten).await;
    assert!(
        live_rows(&rewritten.metadata, fixture.file_io(), None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn zero_output_selected_group_preserves_untouched_data_and_obsolete_deletes() {
    use bergsmith_fixtures::{Commit, FileSpec, TableSpec, row};
    use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
    let schema = Schema::builder()
        .with_fields([NestedField::required(1, "id", Type::Primitive(PrimitiveType::Long)).into()])
        .build()
        .unwrap();
    let fixture = TableSpec::new(schema)
        // Even deletes older than every data file must be retained: retiring
        // them is a separate maintenance operation, outside rewrite authority.
        .commit(Commit::new().file(FileSpec::equality_deletes("obsolete", [1], [row![1 => 99_i64]])))
        .commit(Commit::new()
            .file(FileSpec::data("doomed", (0..2_i64).map(|id| row![1 => id])))
            .file(FileSpec::data("untouched", (0..3_i64).map(|id| row![1 => id]))))
        .commit(Commit::new().file(FileSpec::equality_deletes("shared", [1], [row![1 => 0_i64], row![1 => 1_i64]])))
        .generate().await.unwrap();
    let rewritten = rewrite(&fixture, Some(&["doomed"])).await;
    assert!(rewritten.outputs.is_empty());
    assert_manifests(&fixture, &rewritten).await;
    assert_eq!(
        live_rows(&rewritten.metadata, fixture.file_io(), None)
            .await
            .unwrap(),
        [row![1 => 2_i64]].into_iter().collect()
    );
}

#[tokio::test]
async fn published_manifests_preserve_original_partition_specs() {
    let fixture = spike::original_spec_with_a_different_default()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let rewritten = rewrite(&fixture, Some(&["eu", "us"])).await;
    assert!(!rewritten.outputs.is_empty());
    assert!(
        rewritten
            .outputs
            .iter()
            .all(|file| file.partition_spec_id() == 0)
    );
    assert_eq!(rewritten.metadata.default_partition_spec_id(), 1);
    assert_manifests(&fixture, &rewritten).await;
}

#[tokio::test]
async fn equality_deletes_committed_after_rewrite_apply_to_replacements() {
    use bergsmith_fixtures::{Commit, FileSpec, TableSpec, row};
    use iceberg::transaction::ApplyTransactionAction;
    let fixture = spike::equality_deletes_with_old_and_same_sequence_data()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let rewritten = rewrite(&fixture, None).await;
    // Another writer creates a real equality-delete file, then commits its
    // descriptor after our rewrite using the same capture-only adapter.
    let later = TableSpec::new(fixture.metadata().current_schema().as_ref().clone())
        .commit(Commit::new().file(FileSpec::equality_deletes("later", [1], [row![1 => 3_i64]])))
        .generate()
        .await
        .unwrap();
    let deletes: Vec<_> = entries(later.metadata(), later.file_io())
        .await
        .into_iter()
        .map(|e| e.file)
        .collect();
    let request = baseline(&fixture, &rewritten.metadata)
        .prepare_with(|transaction| {
            transaction
                .overwrite_files()
                .add_data_files(deletes)
                .set_enable_delete_filter_manager(false)
                .apply(transaction)
        })
        .await
        .unwrap();
    let published = publish(&rewritten.metadata, &request);
    let expected = [
        row![1 => 4_i64, 2 => "old", 3 => bergsmith_fixtures::Value::Null],
        row![1 => 1_i64, 2 => "new", 3 => bergsmith_fixtures::Value::Null],
        row![1 => 2_i64, 2 => "new", 3 => bergsmith_fixtures::Value::Null],
        row![1 => 4_i64, 2 => "new", 3 => bergsmith_fixtures::Value::Null],
    ]
    .into_iter()
    .collect();
    assert_eq!(
        live_rows(&published, fixture.file_io(), None)
            .await
            .unwrap(),
        expected
    );
    let selected = rewritten
        .outputs
        .iter()
        .map(|file| file.file_path().to_owned())
        .collect();
    let (_, outputs) = execute_selected(&fixture, &published, &selected).await;
    assert_eq!(
        read_data_files(&published, fixture.file_io(), &outputs)
            .await
            .unwrap(),
        expected
    );
}
