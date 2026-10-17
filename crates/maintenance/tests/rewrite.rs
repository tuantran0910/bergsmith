//! Native rewrite acceptance against real files and the independent oracle.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test setup and assertion helpers fail the test"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::disposition::FailureDisposition;
use bergsmith_common::ids::{AttemptId, JobId, PlanId};
use bergsmith_fixtures::{Fixture, live_rows, read_data_files, spike};
use bergsmith_maintenance::{
    AttemptContext, ExecutionInput, ExecutionLimits, ExecutionRequest, Executor, ExecutorError,
    NativeExecutor, PlanIds, PlanInput, PlanOutcome, Planner, PlannerConfig, ResourceLimit,
};
use bergsmith_protocol::artifact::TaskArtifact;
use bergsmith_protocol::hashing::{artifact_digest, receipt_digest};
use bergsmith_storage::Location;
use uuid::Uuid;

async fn artifacts(fixture: &Fixture) -> Vec<TaskArtifact> {
    let config = PlannerConfig {
        min_input_files: 1,
        min_files_removed: 0,
        ..PlannerConfig::default()
    };
    let PlanOutcome::Planned(plan) = Planner::new(config)
        .unwrap()
        .plan(
            PlanInput {
                metadata: fixture.metadata(),
                metadata_location: Some(fixture.metadata_location()),
                file_io: fixture.file_io(),
                selection: &[],
            },
            PlanIds {
                job_id: JobId::from_uuid(Uuid::from_u128(1)),
                plan_id: PlanId::from_uuid(Uuid::from_u128(2)),
            },
        )
        .await
        .unwrap()
    else {
        panic!("no plan")
    };
    plan.tasks()
        .iter()
        .map(|task| plan.artifact_of(task).unwrap())
        .collect()
}

fn prefix(fixture: &Fixture, artifact: &TaskArtifact, attempt: AttemptId) -> Location {
    let baseline = bergsmith_maintenance::PlanningBaseline::capture(
        fixture.metadata(),
        Some(fixture.metadata_location()),
    )
    .unwrap()
    .unwrap();
    Location::parse(baseline.data_location())
        .unwrap()
        .attempt_output_prefix(artifact.job_id.parse().unwrap(), attempt)
}

async fn execute(
    fixture: &Fixture,
    artifact: &TaskArtifact,
    limits: ExecutionLimits,
) -> Result<bergsmith_maintenance::ExecutionResult, ExecutorError> {
    let attempt_id = AttemptId::from_uuid(Uuid::now_v7());
    let prefix = prefix(fixture, artifact, attempt_id);
    NativeExecutor
        .execute(ExecutionRequest {
            input: ExecutionInput {
                artifact,
                artifact_sha256: artifact_digest(artifact),
                metadata: fixture.metadata(),
                metadata_location: fixture.metadata_location(),
                file_io: fixture.file_io(),
            },
            attempt: AttemptContext {
                attempt_id,
                generation: 1,
            },
            output_prefix: &prefix,
            limits,
        })
        .await
}

fn originals(path: &Path) -> BTreeMap<PathBuf, Sha256Digest> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(originals(&path));
        } else {
            files.insert(
                path.clone(),
                Sha256Digest::of(b"test-file", &std::fs::read(path).unwrap()),
            );
        }
    }
    files
}

fn assert_preserved(files: &BTreeMap<PathBuf, Sha256Digest>) {
    for (path, digest) in files {
        assert_eq!(
            *digest,
            Sha256Digest::of(b"test-file", &std::fs::read(path).unwrap()),
            "{}",
            path.display()
        );
    }
}

async fn certify(spike: spike::SpikeFixture) {
    let fixture = spike.generate().await.unwrap();
    let original = originals(fixture.root());
    for mut artifact in artifacts(&fixture).await {
        if let Some(selection) = &spike.selection {
            let paths: BTreeSet<_> = selection
                .iter()
                .map(|name| fixture.path_of(name).unwrap())
                .collect();
            artifact
                .source_files
                .retain(|file| paths.contains(file.path.as_str()));
            if artifact.source_files.is_empty() {
                continue;
            }
        }
        let selected = artifact
            .source_files
            .iter()
            .map(|source| source.path.clone())
            .collect();
        let expected = live_rows(fixture.metadata(), fixture.file_io(), Some(&selected))
            .await
            .unwrap();
        let result = execute(&fixture, &artifact, ExecutionLimits::default())
            .await
            .unwrap();
        assert_eq!(
            read_data_files(fixture.metadata(), fixture.file_io(), result.data_files())
                .await
                .unwrap(),
            expected,
            "{}",
            spike.name
        );
        let receipt = result.receipt();
        assert_eq!(result.receipt_sha256(), receipt_digest(receipt));
        assert_eq!(
            receipt.artifact_sha256,
            artifact_digest(&artifact).as_bytes()
        );
        let metrics = receipt.metrics.as_ref().unwrap();
        assert_eq!(u64::try_from(metrics.rows_written).unwrap(), expected.len());
        assert_eq!(
            metrics.rows_read,
            artifact
                .source_files
                .iter()
                .map(|f| f.record_count)
                .sum::<i64>()
        );
        assert_eq!(
            metrics.rows_read - metrics.rows_deleted,
            metrics.rows_written
        );
        assert_eq!(
            metrics.bytes_written,
            receipt
                .outputs
                .iter()
                .map(|f| f.file_size_bytes)
                .sum::<i64>()
        );
        assert!(metrics.bytes_read > 0);
        assert_eq!(receipt.outputs.len(), result.data_files().len());
        let root = prefix(&fixture, &artifact, receipt.attempt_id.parse().unwrap());
        for (file, output) in result.data_files().iter().zip(&receipt.outputs) {
            assert!(Location::parse(file.file_path()).unwrap().is_within(&root));
            assert_eq!(
                file.partition_spec_id(),
                artifact.output.as_ref().unwrap().partition_spec_id
            );
            assert_eq!(
                output.partition,
                artifact.output.as_ref().unwrap().partition
            );
            assert_eq!(
                output.record_count,
                i64::try_from(file.record_count()).unwrap()
            );
            assert_eq!(
                std::fs::metadata(file.file_path().strip_prefix("file://").unwrap())
                    .unwrap()
                    .len(),
                u64::try_from(output.file_size_bytes).unwrap()
            );
            assert!(!output.columns.is_empty());
            assert!(!output.split_offsets.is_empty());
            assert!(
                output
                    .columns
                    .windows(2)
                    .all(|pair| pair[0].field_id < pair[1].field_id)
            );
        }
    }
    assert_preserved(&original);
}

#[tokio::test]
async fn small_files_preserve_duplicates_and_nulls() {
    certify(spike::small_files_with_duplicates_and_nulls().unwrap()).await;
}
#[tokio::test]
async fn positions_across_row_groups_are_applied() {
    certify(spike::position_deletes_across_row_groups().unwrap()).await;
}
#[tokio::test]
async fn equality_deletes_keep_same_sequence_data() {
    certify(spike::equality_deletes_with_old_and_same_sequence_data().unwrap()).await;
}
#[tokio::test]
async fn nullable_single_keys_match_the_oracle() {
    certify(spike::nullable_single_equality_key().unwrap()).await;
}
#[tokio::test]
async fn nullable_composite_keys_match_the_oracle() {
    certify(spike::composite_nullable_equality_keys().unwrap()).await;
}
#[tokio::test]
async fn shared_deletes_and_untouched_sources_are_preserved() {
    certify(spike::shared_deletes_with_an_untouched_file().unwrap()).await;
}
#[tokio::test]
async fn fully_deleted_groups_have_no_replacement_files() {
    certify(spike::fully_deleted_group().unwrap()).await;
}
#[tokio::test]
async fn original_partition_specs_are_preserved() {
    certify(spike::original_spec_with_a_different_default().unwrap()).await;
}
#[tokio::test]
async fn renamed_and_added_columns_match_by_id() {
    certify(spike::renamed_field_and_added_column().unwrap()).await;
}
#[tokio::test]
async fn fifty_thousand_equality_keys_match_the_oracle() {
    certify(spike::large_equality_delete().unwrap()).await;
}
#[tokio::test]
async fn wide_rows_stream_with_bounded_batches() {
    certify(spike::wide_rows().unwrap()).await;
}

#[tokio::test]
async fn low_resource_budgets_fail_without_a_receipt_and_preserve_sources() {
    let fixture = spike::fully_deleted_group()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let artifact = &artifacts(&fixture).await[0];
    let original = originals(fixture.root());
    for (limits, expected) in [
        (
            ExecutionLimits {
                delete_memory_bytes: 1,
                ..ExecutionLimits::default()
            },
            ResourceLimit::DeleteMemory,
        ),
        (
            ExecutionLimits {
                decode_row_group_bytes: 1,
                ..ExecutionLimits::default()
            },
            ResourceLimit::DecodeRowGroup,
        ),
        (
            ExecutionLimits {
                read_request_bytes: 1,
                ..ExecutionLimits::default()
            },
            ResourceLimit::ReadBuffer,
        ),
        (
            ExecutionLimits {
                footer_bytes: 1,
                ..ExecutionLimits::default()
            },
            ResourceLimit::Footer,
        ),
    ] {
        let error = execute(&fixture, artifact, limits).await.err().unwrap();
        assert!(
            matches!(error, ExecutorError::ResourceLimit(limit) if limit == expected),
            "{error}"
        );
        assert_eq!(error.disposition(), FailureDisposition::PermanentFailure);
        assert_eq!(error.to_failure().reason_code, expected.reason_code());
    }
    assert_preserved(&original);
}

#[tokio::test]
async fn output_bytes_batch_memory_and_file_inventory_are_enforced() {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let mut artifact = artifacts(&fixture).await.remove(0);
    artifact.output.as_mut().unwrap().target_file_size_bytes = 1;
    let original = originals(fixture.root());
    for (limits, expected) in [
        (
            ExecutionLimits {
                output_bytes: 1,
                ..ExecutionLimits::default()
            },
            ResourceLimit::OutputBytes,
        ),
        (
            ExecutionLimits {
                batch_memory_bytes: 1,
                ..ExecutionLimits::default()
            },
            ResourceLimit::BatchMemory,
        ),
        (
            ExecutionLimits {
                output_files: 1,
                ..ExecutionLimits::default()
            },
            ResourceLimit::OutputFiles,
        ),
    ] {
        let error = execute(&fixture, &artifact, limits).await.err().unwrap();
        assert!(
            matches!(error, ExecutorError::ResourceLimit(limit) if limit == expected),
            "{error}"
        );
    }
    assert_preserved(&original);
}

#[tokio::test]
async fn rolling_recomputes_each_files_metrics_and_refuses_attempt_reuse() {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let mut artifact = artifacts(&fixture).await.remove(0);
    artifact.output.as_mut().unwrap().target_file_size_bytes = 1;
    let result = execute(&fixture, &artifact, ExecutionLimits::default())
        .await
        .unwrap();
    assert!(result.data_files().len() > 1);
    assert_eq!(
        read_data_files(fixture.metadata(), fixture.file_io(), result.data_files())
            .await
            .unwrap(),
        fixture.live_rows().await.unwrap()
    );
    for file in result.data_files() {
        assert_eq!(file.record_count(), 4);
        assert_eq!(file.value_counts()[&1], 4);
        assert_eq!(file.null_value_counts()[&2], 2);
        assert!(file.lower_bounds().contains_key(&1));
        assert!(file.upper_bounds().contains_key(&1));
    }
    let attempt_id = result.receipt().attempt_id.parse().unwrap();
    let root = prefix(&fixture, &artifact, attempt_id);
    let before = originals(fixture.root());
    let retry = NativeExecutor
        .execute(ExecutionRequest {
            input: ExecutionInput {
                artifact: &artifact,
                artifact_sha256: artifact_digest(&artifact),
                metadata: fixture.metadata(),
                metadata_location: fixture.metadata_location(),
                file_io: fixture.file_io(),
            },
            attempt: AttemptContext {
                attempt_id,
                generation: 1,
            },
            output_prefix: &root,
            limits: ExecutionLimits::default(),
        })
        .await;
    assert!(retry.is_err());
    assert_preserved(&before);
}

#[tokio::test]
async fn output_prefix_and_generation_are_validated_before_writes() {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let artifact = artifacts(&fixture).await.remove(0);
    let attempt_id = AttemptId::from_uuid(Uuid::now_v7());
    let correct = prefix(&fixture, &artifact, attempt_id);
    let wrong = correct.child("other").unwrap();
    let before = originals(fixture.root());
    for (root, generation) in [(&wrong, 1), (&correct, 0)] {
        let error = NativeExecutor
            .execute(ExecutionRequest {
                input: ExecutionInput {
                    artifact: &artifact,
                    artifact_sha256: artifact_digest(&artifact),
                    metadata: fixture.metadata(),
                    metadata_location: fixture.metadata_location(),
                    file_io: fixture.file_io(),
                },
                attempt: AttemptContext {
                    attempt_id,
                    generation,
                },
                output_prefix: root,
                limits: ExecutionLimits::default(),
            })
            .await
            .err()
            .unwrap();
        assert!(matches!(
            error,
            ExecutorError::OutputPrefix | ExecutorError::InvalidArtifact(_)
        ));
    }
    assert_eq!(before, originals(fixture.root()));
}

#[tokio::test]
async fn nan_rows_are_preserved_and_nan_metrics_are_recomputed() {
    use bergsmith_fixtures::{Commit, FileSpec, TableSpec, row};
    use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
    use std::sync::Arc;
    let schema = Schema::builder()
        .with_fields([
            Arc::new(NestedField::required(
                1,
                "id",
                Type::Primitive(PrimitiveType::Long),
            )),
            Arc::new(NestedField::optional(
                2,
                "value",
                Type::Primitive(PrimitiveType::Double),
            )),
        ])
        .build()
        .unwrap();
    let mut commit = Commit::new();
    for id in 0..5 {
        commit = commit.file(FileSpec::data(
            format!("n{id}"),
            [row![1 => i64::from(id), 2 => bergsmith_fixtures::Value::double(f64::NAN)]],
        ));
    }
    let fixture = TableSpec::new(schema)
        .commit(commit)
        .generate()
        .await
        .unwrap();
    let artifact = artifacts(&fixture).await.remove(0);
    let result = execute(&fixture, &artifact, ExecutionLimits::default())
        .await
        .unwrap();
    assert_eq!(
        read_data_files(fixture.metadata(), fixture.file_io(), result.data_files())
            .await
            .unwrap(),
        fixture.live_rows().await.unwrap()
    );
    assert_eq!(
        result.receipt().outputs[0]
            .columns
            .iter()
            .find(|c| c.field_id == 2)
            .unwrap()
            .nan_value_count,
        Some(5)
    );
}
