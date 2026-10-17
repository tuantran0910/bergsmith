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

/// Check explicit expectations against both the independent oracle and every
/// planned native group, including groups in different partition specs.
async fn certify_table(
    table: bergsmith_fixtures::TableSpec,
    expected: bergsmith_fixtures::RowMultiset,
) {
    let fixture = table.generate().await.unwrap();
    let original = originals(fixture.root());
    assert_eq!(fixture.live_rows().await.unwrap(), expected);
    let tasks = artifacts(&fixture).await;
    assert!(!tasks.is_empty());
    let mut outputs = Vec::new();
    for artifact in tasks {
        let selected = artifact
            .source_files
            .iter()
            .map(|f| f.path.clone())
            .collect();
        let expected_group = live_rows(fixture.metadata(), fixture.file_io(), Some(&selected))
            .await
            .unwrap();
        let result = execute(&fixture, &artifact, ExecutionLimits::default())
            .await
            .unwrap();
        let rows = read_data_files(fixture.metadata(), fixture.file_io(), result.data_files())
            .await
            .unwrap();
        assert_eq!(rows, expected_group);
        outputs.extend_from_slice(result.data_files());
    }
    assert_eq!(
        read_data_files(fixture.metadata(), fixture.file_io(), &outputs)
            .await
            .unwrap(),
        expected
    );
    assert_preserved(&original);
}

#[tokio::test]
async fn composite_null_keys_match_all_null_patterns_and_preserve_duplicate_survivors() {
    use bergsmith_fixtures::{Commit, FileSpec, TableSpec, Value, row};
    use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
    let schema = Schema::builder()
        .with_fields((1..=2).map(|id| {
            NestedField::optional(id, format!("k{id}"), Type::Primitive(PrimitiveType::Long)).into()
        }))
        .build()
        .unwrap();
    let rows: Vec<_> = [None, Some(1_i64), Some(2)]
        .into_iter()
        .flat_map(|a| {
            [None, Some(1_i64), Some(2)]
                .into_iter()
                .map(move |b| row![1 => a, 2 => b])
        })
        .collect();
    let keys = [
        row![1 => Value::Null, 2 => Value::Null],
        row![1 => Value::Null, 2 => 1_i64],
        row![1 => 1_i64, 2 => Value::Null],
        row![1 => 1_i64, 2 => 1_i64],
    ];
    let survivors: Vec<_> = rows
        .iter()
        .filter(|row| !keys.contains(row))
        .cloned()
        .collect();
    let expected = survivors
        .iter()
        .cloned()
        .chain(survivors.clone())
        .chain(rows.clone())
        .collect();
    certify_table(
        TableSpec::new(schema)
            .commit(Commit::new().file(FileSpec::data(
                "old",
                rows.iter().cloned().chain(rows.clone()),
            )))
            .commit(
                Commit::new()
            .file(FileSpec::data("same-sequence", rows))
            // Declaration order differs from schema order; match by field ID.
            .file(FileSpec::equality_deletes("keys", [2, 1], keys)),
            ),
        expected,
    )
    .await;
}

#[tokio::test]
async fn sequence_boundaries_distinguish_position_and_equality_deletes() {
    use bergsmith_fixtures::{Commit, FileSpec, TableSpec, row};
    use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
    let schema = Schema::builder()
        .with_fields([NestedField::required(1, "id", Type::Primitive(PrimitiveType::Long)).into()])
        .build()
        .unwrap();
    let rows = || (0..4_i64).map(|id| row![1 => id]);
    // Sequence 1 data sees both deletes; sequence 2 sees only its position
    // delete; sequence 3 sees neither. Duplicate IDs expose over-deletion.
    let expected = [1_i64, 3, 0, 1, 3, 0, 1, 2, 3]
        .into_iter()
        .map(|id| row![1 => id])
        .collect();
    certify_table(
        TableSpec::new(schema)
            .commit(Commit::new().file(FileSpec::data("old", rows())))
            .commit(
                Commit::new()
                    .file(FileSpec::data("same", rows()))
                    .file(FileSpec::position_deletes(
                        "positions",
                        [("old", 2), ("same", 2)],
                    ))
                    .file(FileSpec::equality_deletes("keys", [1], [row![1 => 0_i64]])),
            )
            .commit(Commit::new().file(FileSpec::data("later", rows()))),
        expected,
    )
    .await;
}

#[tokio::test]
async fn global_deletes_cross_specs_but_partitioned_deletes_stay_in_scope() {
    use bergsmith_fixtures::{Commit, FileSpec, TableSpec, Value, row};
    use iceberg::spec::{
        NestedField, PrimitiveType, Schema, Transform, Type, UnboundPartitionSpec,
    };
    let schema = Schema::builder()
        .with_fields([
            NestedField::required(1, "id", Type::Primitive(PrimitiveType::Long)).into(),
            NestedField::required(2, "tag", Type::Primitive(PrimitiveType::String)).into(),
        ])
        .build()
        .unwrap();
    let spec = UnboundPartitionSpec::builder()
        .add_partition_field(2, "tag", Transform::Identity)
        .unwrap()
        .build();
    let data = |name: &str, tag: &str| {
        FileSpec::data(name, (0..4_i64).map(|id| row![1 => id, 2 => tag]))
            .partition([Value::string(tag)])
    };
    let expected = [
        row![1 => 2_i64, 2 => "x"],
        row![1 => 3_i64, 2 => "x"],
        row![1 => 1_i64, 2 => "y"],
        row![1 => 2_i64, 2 => "y"],
        row![1 => 3_i64, 2 => "y"],
    ]
    .into_iter()
    .collect();
    certify_table(
        TableSpec::new(schema)
            .partitioned_by(spec)
            .commit(Commit::new().file(data("x", "x")).file(data("y", "y")))
            .commit(
                Commit::new().file(
                    FileSpec::equality_deletes("local", [1], [row![1 => 1_i64]])
                        .partition([Value::string("x")]),
                ),
            )
            .commit(
                Commit::new()
                    .evolve_spec(UnboundPartitionSpec::builder().build())
                    .file(FileSpec::equality_deletes(
                        "global",
                        [1],
                        [row![1 => 0_i64]],
                    ))
                    .file(FileSpec::position_deletes("global-position", [("x", 2)])),
            ),
        expected,
    )
    .await;
}

#[tokio::test]
async fn supported_deletes_preserve_float_and_double_nan_payloads() {
    use bergsmith_fixtures::{Commit, FileSpec, TableSpec, Value, row};
    use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
    let schema = Schema::builder()
        .with_fields([
            NestedField::required(1, "id", Type::Primitive(PrimitiveType::Long)).into(),
            NestedField::optional(2, "f", Type::Primitive(PrimitiveType::Float)).into(),
            NestedField::optional(3, "d", Type::Primitive(PrimitiveType::Double)).into(),
        ])
        .build()
        .unwrap();
    let rows = [
        row![1 => 0_i64, 2 => Value::float(1.0), 3 => Value::double(2.0)],
        row![1 => 1_i64, 2 => Value::Float(0x7fc0_1234), 3 => Value::Double(0x7ff8_0000_0000_1234)],
        row![1 => 2_i64, 2 => Value::float(f32::NAN), 3 => Value::double(f64::NAN)],
        row![1 => 3_i64, 2 => Value::Null, 3 => Value::Null],
    ];
    let expected = rows[1..3].iter().cloned().collect();
    certify_table(
        TableSpec::new(schema)
            .commit(Commit::new().file(FileSpec::data("data", rows)))
            .commit(
                Commit::new()
                    .file(FileSpec::position_deletes("positions", [("data", 0)]))
                    .file(FileSpec::equality_deletes("keys", [1], [row![1 => 3_i64]])),
            ),
        expected,
    )
    .await;
}

fn unsupported_key_cases() -> [(
    bergsmith_fixtures::TableSpec,
    bergsmith_maintenance::KeyProblem,
); 4] {
    use bergsmith_fixtures::{Commit, FileSpec, TableSpec, Value, row};
    use bergsmith_maintenance::KeyProblem;
    use iceberg::spec::{NestedField, PrimitiveType, Schema, StructType, Type};
    let id = || NestedField::required(1, "id", Type::Primitive(PrimitiveType::Long));
    let schema = |key_type| {
        Schema::builder()
            .with_fields([
                id().into(),
                NestedField::optional(2, "key", key_type).into(),
            ])
            .build()
            .unwrap()
    };
    let string_key = || {
        TableSpec::new(schema(Type::Primitive(PrimitiveType::String)))
            .commit(Commit::new().file(FileSpec::equality_deletes("keys", [2], [row![2 => "x"]])))
    };
    [
        (
            TableSpec::new(schema(Type::Primitive(PrimitiveType::Float))).commit(
                Commit::new().file(FileSpec::equality_deletes(
                    "keys",
                    [2],
                    [row![2 => Value::float(f32::NAN)]],
                )),
            ),
            KeyProblem::FloatingPoint,
        ),
        (
            TableSpec::new(schema(Type::Primitive(PrimitiveType::Double))).commit(
                Commit::new().file(FileSpec::equality_deletes(
                    "keys",
                    [2],
                    [row![2 => Value::double(1.0)]],
                )),
            ),
            KeyProblem::FloatingPoint,
        ),
        (
            string_key().commit(
                Commit::new().evolve_schema(
                    Schema::builder()
                        .with_fields([id().into()])
                        .build()
                        .unwrap(),
                ),
            ),
            KeyProblem::NotInSchema,
        ),
        (
            string_key().commit(
                Commit::new().evolve_schema(
                    Schema::builder()
                        .with_fields([
                            id().into(),
                            NestedField::optional(
                                3,
                                "outer",
                                Type::Struct(StructType::new(vec![
                                    NestedField::optional(
                                        2,
                                        "key",
                                        Type::Primitive(PrimitiveType::String),
                                    )
                                    .into(),
                                ])),
                            )
                            .into(),
                        ])
                        .build()
                        .unwrap(),
                ),
            ),
            KeyProblem::Nested,
        ),
    ]
}

#[tokio::test]
async fn unsupported_keys_are_refused_by_both_planner_and_worker_before_writes() {
    use bergsmith_maintenance::Rejection;
    use bergsmith_protocol::artifact::{Baseline, OutputSpec, PartitionKey, SourceFile, TableRef};
    for (table, problem) in unsupported_key_cases() {
        let fixture = table.generate().await.unwrap();
        let original = originals(fixture.root());
        let planner = Planner::new(PlannerConfig::default()).unwrap();
        let outcome = planner
            .plan(
                PlanInput {
                    metadata: fixture.metadata(),
                    metadata_location: Some(fixture.metadata_location()),
                    file_io: fixture.file_io(),
                    selection: &[],
                },
                PlanIds {
                    job_id: JobId::generate(),
                    plan_id: PlanId::generate(),
                },
            )
            .await
            .unwrap();
        let expected = vec![Rejection::EqualityKey {
            path: fixture.path_of("keys").unwrap().into(),
            field_id: 2,
            problem,
        }];
        assert_eq!(outcome, PlanOutcome::Rejected(expected.clone()));
        let metadata = fixture.metadata();
        let snapshot = metadata.current_snapshot().unwrap();
        // A caller cannot bypass preview by constructing its own artifact.
        // The source is deliberately unreadable; unsupported keys must be
        // rejected from metadata before source validation or output writes.
        let artifact = TaskArtifact {
            job_id: JobId::generate().to_string(),
            plan_id: PlanId::generate().to_string(),
            task_id: Uuid::now_v7().to_string(),
            table: Some(TableRef {
                table_uuid: metadata.uuid().to_string(),
                location: metadata.location().into(),
                format_version: 2,
            }),
            baseline: Some(Baseline {
                snapshot_id: snapshot.snapshot_id(),
                sequence_number: snapshot.sequence_number(),
                schema_id: metadata.current_schema_id(),
            }),
            source_files: vec![SourceFile {
                path: "file:///unreadable.parquet".into(),
                ..Default::default()
            }],
            output: Some(OutputSpec {
                partition: Some(PartitionKey::default()),
                target_file_size_bytes: 1024,
                data_sequence_number: snapshot.sequence_number(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let error = execute(&fixture, &artifact, ExecutionLimits::default())
            .await
            .err()
            .unwrap();
        assert!(
            matches!(&error, ExecutorError::Unsupported(rejections) if *rejections == expected),
            "{error}"
        );
        assert_eq!(error.disposition(), FailureDisposition::PermanentFailure);
        assert_eq!(original, originals(fixture.root()));
    }
}
