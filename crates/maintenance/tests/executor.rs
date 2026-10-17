//! BS-010.T1: planned identities are checked before native row reads.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test helpers fail the test"
)]

mod support;

use std::collections::BTreeSet;

use arrow_array::Array;
use arrow_array::cast::AsArray;
use arrow_array::types::{Int32Type, Int64Type};
use bergsmith_common::ids::{JobId, PlanId};
use bergsmith_fixtures::{Commit, Fixture, Row, RowMultiset, TableSpec, Value, spike};
use bergsmith_maintenance::{
    ExecutionInput, Executor, ExecutorError, NativeExecutor, PlanDocument, PlanIds, PlanInput,
    PlanOutcome, PlannedDelete, PlannedFile, Planner, PlannerConfig, PreparedScan,
};
use bergsmith_protocol::artifact::TaskArtifact;
use bergsmith_protocol::hashing::artifact_digest;
use futures::TryStreamExt;
use iceberg::spec::{NestedField, PrimitiveType, TableMetadata, TableMetadataBuilder, Type};
use uuid::Uuid;

async fn plan(fixture: &Fixture) -> PlanDocument {
    plan_with_metadata(fixture, fixture.metadata()).await
}

async fn plan_with_metadata(fixture: &Fixture, metadata: &TableMetadata) -> PlanDocument {
    let config = PlannerConfig {
        min_input_files: 1,
        min_files_removed: 0,
        ..PlannerConfig::default()
    };
    let outcome = Planner::new(config)
        .unwrap()
        .plan(
            PlanInput {
                metadata,
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
        .unwrap();
    let PlanOutcome::Planned(plan) = outcome else {
        panic!("{outcome:?}")
    };
    *plan
}

async fn prepare(
    artifact: &TaskArtifact,
    fixture: &Fixture,
) -> Result<PreparedScan, ExecutorError> {
    prepare_with_metadata(artifact, fixture, fixture.metadata()).await
}

async fn prepare_with_metadata(
    artifact: &TaskArtifact,
    fixture: &Fixture,
    metadata: &TableMetadata,
) -> Result<PreparedScan, ExecutorError> {
    NativeExecutor
        .prepare(ExecutionInput {
            artifact,
            artifact_sha256: artifact_digest(artifact),
            metadata,
            metadata_location: fixture.metadata_location(),
            file_io: fixture.file_io(),
        })
        .await
}

#[tokio::test]
async fn all_spike_fixtures_prepare_exact_whole_files_with_original_partitions() {
    for spike in spike::all().unwrap() {
        let fixture = spike.generate().await.unwrap();
        let plan = plan(&fixture).await;
        for task in plan.tasks() {
            let artifact = plan.artifact_of(task).unwrap();
            let prepared = prepare(&artifact, &fixture).await.unwrap();
            let paths: Vec<_> = prepared
                .files()
                .iter()
                .map(|f| f.data_file_path.as_str())
                .collect();
            assert_eq!(
                paths,
                task.sources()
                    .iter()
                    .map(PlannedFile::path)
                    .collect::<Vec<_>>(),
                "{}",
                spike.name
            );
            let deletes: BTreeSet<_> = prepared
                .files()
                .iter()
                .flat_map(|f| f.deletes.iter().map(|d| d.file_path.as_str()))
                .collect();
            assert_eq!(
                deletes,
                task.deletes().iter().map(PlannedDelete::path).collect(),
                "{}",
                spike.name
            );
            for (file, source) in prepared.files().iter().zip(&artifact.source_files) {
                assert_eq!(file.start, 0);
                assert_eq!(file.length, u64::try_from(source.file_size_bytes).unwrap());
                assert_eq!(
                    file.record_count,
                    Some(u64::try_from(source.record_count).unwrap())
                );
                assert_eq!(file.sequence_number, source.data_sequence_number);
                assert_eq!(
                    file.partition_spec.as_ref().unwrap().spec_id(),
                    task.partition().spec_id()
                );
                assert!(file.predicate.is_none());
                assert_eq!(file.schema.schema_id(), plan.baseline().schema_id());
            }
        }
    }
}

#[tokio::test]
async fn a_changed_artifact_is_rejected_against_the_grant_digest() {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let plan = plan(&fixture).await;
    let mut artifact = plan.artifact_of(&plan.tasks()[0]).unwrap();
    let digest = artifact_digest(&artifact);
    artifact.source_files[0].record_count += 1;
    let result = NativeExecutor
        .prepare(ExecutionInput {
            artifact: &artifact,
            artifact_sha256: digest,
            metadata: fixture.metadata(),
            metadata_location: fixture.metadata_location(),
            file_io: fixture.file_io(),
        })
        .await;
    assert!(matches!(result, Err(ExecutorError::ArtifactDigest)));
}

#[tokio::test]
async fn canonical_source_order_is_independent_of_download_order() {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let plan = plan(&fixture).await;
    let mut artifact = plan.artifact_of(&plan.tasks()[0]).unwrap();
    let expected = prepare(&artifact, &fixture).await.unwrap();
    artifact.source_files.reverse();
    let reordered = prepare(&artifact, &fixture).await.unwrap();
    assert_eq!(expected.files(), reordered.files());
}

#[tokio::test]
async fn unsupported_metadata_is_rejected_before_reading_rows() {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let plan = plan(&fixture).await;
    let artifact = plan.artifact_of(&plan.tasks()[0]).unwrap();
    let metadata = support::with_properties(fixture.metadata(), &[("encryption.key-id", "key")]);
    assert!(matches!(
        prepare_with_metadata(&artifact, &fixture, &metadata).await,
        Err(ExecutorError::Unsupported(_))
    ));
}

#[tokio::test]
async fn source_identity_mismatches_and_duplicates_are_rejected() {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let plan = plan(&fixture).await;
    let artifact = plan.artifact_of(&plan.tasks()[0]).unwrap();
    for case in 0..7 {
        let mut changed = artifact.clone();
        let source = &mut changed.source_files[0];
        match case {
            0 => source.path.push_str("-missing"),
            1 => source.file_size_bytes += 1,
            2 => source.record_count += 1,
            3 => source.data_sequence_number += 1,
            4 => source.partition.as_mut().unwrap().spec_id += 1,
            5 => source.file_size_bytes = -1,
            6 => changed.source_files.push(changed.source_files[0].clone()),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                prepare(&changed, &fixture).await,
                Err(ExecutorError::FileIdentity { .. })
            ),
            "case {case}"
        );
    }
}

#[tokio::test]
async fn omitted_changed_and_duplicate_deletes_are_rejected() {
    let fixture = spike::nullable_single_equality_key()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let plan = plan(&fixture).await;
    let artifact = plan.artifact_of(&plan.tasks()[0]).unwrap();
    for case in 0..9 {
        let mut changed = artifact.clone();
        match case {
            0 => changed.delete_files.clear(),
            1 => changed.delete_files[0].path.push_str("-missing"),
            2 => changed.delete_files[0].file_size_bytes += 1,
            3 => changed.delete_files[0].record_count += 1,
            4 => changed.delete_files[0].data_sequence_number += 1,
            5 => changed.delete_files[0].content = 0,
            6 => changed.delete_files[0].equality_field_ids.push(99),
            7 => changed.delete_files[0].partition = None,
            8 => changed.delete_files.push(changed.delete_files[0].clone()),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                prepare(&changed, &fixture).await,
                Err(ExecutorError::FileIdentity { .. } | ExecutorError::DeleteInventory)
            ),
            "case {case}"
        );
    }
}

#[tokio::test]
async fn invalid_baselines_and_output_groups_are_rejected() {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let plan = plan(&fixture).await;
    let artifact = plan.artifact_of(&plan.tasks()[0]).unwrap();
    for case in 0..14 {
        let mut changed = artifact.clone();
        match case {
            0 => changed.table = None,
            1 => changed.baseline = None,
            2 => changed.output = None,
            3 => changed.table.as_mut().unwrap().table_uuid = Uuid::nil().to_string(),
            4 => changed.table.as_mut().unwrap().location.push_str("/other"),
            5 => changed.baseline.as_mut().unwrap().snapshot_id += 1,
            6 => changed.baseline.as_mut().unwrap().sequence_number += 1,
            7 => changed.baseline.as_mut().unwrap().schema_id += 1,
            8 => changed.task_id.clear(),
            9 => changed.output.as_mut().unwrap().partition_spec_id += 1,
            10 => changed.output.as_mut().unwrap().partition = None,
            11 => changed.output.as_mut().unwrap().target_file_size_bytes = 0,
            12 => changed.output.as_mut().unwrap().data_sequence_number += 1,
            13 => changed.source_files.clear(),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                prepare(&changed, &fixture).await,
                Err(ExecutorError::BaselineMismatch | ExecutorError::InvalidArtifact(_))
            ),
            "case {case}"
        );
    }
}

async fn rows(prepared: PreparedScan) -> RowMultiset {
    let mut stream = prepared.read().unwrap().stream();
    let mut rows = RowMultiset::new();
    while let Some(batch) = stream.try_next().await.unwrap() {
        for row in 0..batch.num_rows() {
            rows.insert(Row::new(batch.columns().iter().enumerate().map(
                |(index, array)| {
                    let schema = batch.schema();
                    let field = &schema.fields()[index];
                    let id = field.metadata()["PARQUET:field_id"].parse::<i32>().unwrap();
                    let value = if array.is_null(row) {
                        Value::Null
                    } else if let Some(array) = array.as_primitive_opt::<Int64Type>() {
                        Value::Long(array.value(row))
                    } else if let Some(array) = array.as_primitive_opt::<Int32Type>() {
                        Value::Int(array.value(row))
                    } else if let Some(array) = array.as_string_opt::<i32>() {
                        Value::string(array.value(row))
                    } else {
                        panic!("unsupported test column {}", array.data_type())
                    };
                    (id, value)
                },
            )));
        }
    }
    rows
}

#[tokio::test]
async fn native_rows_match_the_oracle_for_duplicates_positions_and_schema_evolution() {
    for spike in [
        spike::small_files_with_duplicates_and_nulls().unwrap(),
        spike::position_deletes_across_row_groups().unwrap(),
        spike::original_spec_with_a_different_default().unwrap(),
        spike::renamed_field_and_added_column().unwrap(),
    ] {
        let fixture = spike.generate().await.unwrap();
        let plan = plan(&fixture).await;
        let mut actual = RowMultiset::new();
        for task in plan.tasks() {
            let artifact = plan.artifact_of(task).unwrap();
            let read = rows(prepare(&artifact, &fixture).await.unwrap()).await;
            for (row, count) in read.excess_over(&RowMultiset::new()) {
                for _ in 0..count {
                    actual.insert(row.clone());
                }
            }
        }
        assert_eq!(actual, fixture.live_rows().await.unwrap(), "{}", spike.name);
    }
}

#[tokio::test]
async fn a_task_reads_only_its_partition_and_keeps_all_its_rows() {
    let fixture = TableSpec::new(support::tagged())
        .partitioned_by(support::by_tag())
        .commit(
            Commit::new()
                .file(support::data("x", 5).partition([Value::string("x")]))
                .file(support::data("y", 3).partition([Value::string("y")])),
        )
        .generate()
        .await
        .unwrap();
    let plan = plan(&fixture).await;
    assert_eq!(plan.tasks().len(), 2);
    for task in plan.tasks() {
        let artifact = plan.artifact_of(task).unwrap();
        let actual = rows(prepare(&artifact, &fixture).await.unwrap()).await;
        let source = task.sources()[0].path();
        let name = if source == fixture.path_of("x").unwrap() {
            "x"
        } else {
            "y"
        };
        assert_eq!(actual, fixture.live_rows_of(&[name]).await.unwrap());
    }
}

#[tokio::test]
async fn metadata_only_schema_evolution_uses_the_planned_current_schema() {
    let fixture = TableSpec::new(support::tagged())
        .commit(Commit::new().file(support::data("before", 3)))
        .generate()
        .await
        .unwrap();
    let after = support::schema([
        NestedField::required(1, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::optional(2, "renamed_tag", Type::Primitive(PrimitiveType::String)),
        NestedField::optional(3, "added", Type::Primitive(PrimitiveType::Int)),
    ]);
    let metadata = TableMetadataBuilder::new_from_metadata(fixture.metadata().clone(), None)
        .add_schema(after)
        .unwrap()
        .set_current_schema(-1)
        .unwrap()
        .build()
        .unwrap()
        .metadata;
    let plan = plan_with_metadata(&fixture, &metadata).await;
    let artifact = plan.artifact_of(&plan.tasks()[0]).unwrap();
    let prepared = prepare_with_metadata(&artifact, &fixture, &metadata)
        .await
        .unwrap();
    let schema = &prepared.files()[0].schema;
    assert_eq!(schema.schema_id(), metadata.current_schema_id());
    assert_eq!(schema.field_by_id(2).unwrap().name, "renamed_tag");
    assert_eq!(
        rows(prepared).await,
        bergsmith_fixtures::live_rows(&metadata, fixture.file_io(), None)
            .await
            .unwrap()
    );
}
