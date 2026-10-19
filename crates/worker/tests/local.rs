//! Local mode runs a hashed plan without database or catalog access.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "fixture and assertion helpers fail the test"
)]

use std::collections::HashMap;
use std::path::PathBuf;

use bergsmith_common::disposition::FailureDisposition;
use bergsmith_common::ids::{JobId, PlanId};
use bergsmith_fixtures::spike;
use bergsmith_maintenance::{
    ExecutorError, PlanIds, PlanInput, PlanOutcome, Planner, PlannerConfig, ResourceLimit,
};
use bergsmith_protocol::artifact::TaskReceipt;
use bergsmith_protocol::hashing::receipt_digest;
use bergsmith_worker::local::{self, LocalError, LocalRequest};
use iceberg::spec::TableMetadataBuilder;
use prost::Message;
use uuid::Uuid;

async fn setup() -> (bergsmith_fixtures::Fixture, LocalRequest) {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let PlanOutcome::Planned(plan) = Planner::new(PlannerConfig::default())
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
    let plan_file = fixture.root().join("plan.json");
    std::fs::write(&plan_file, plan.to_json().unwrap()).unwrap();
    let request = LocalRequest {
        plan_file,
        plan_sha256: plan.sha256().unwrap().to_hex(),
        receipt_dir: fixture.root().join("receipts"),
        limits_file: None,
    };
    (fixture, request)
}

#[tokio::test]
async fn local_execution_persists_verifiable_protobuf_receipts_without_publication() {
    let (fixture, request) = setup().await;
    let before =
        std::fs::read(fixture.metadata_location().strip_prefix("file://").unwrap()).unwrap();
    let summary = request.receipt_dir.join("summary.json");
    let run = local::run(request).await.unwrap();
    assert_eq!(run.receipts.len(), 1);
    let result = &run.receipts[0];
    let receipt =
        TaskReceipt::decode(std::fs::read(&result.receipt_file).unwrap().as_slice()).unwrap();
    assert_eq!(receipt_digest(&receipt).to_hex(), result.receipt_sha256);
    assert_eq!(result.rows_read, 40);
    assert_eq!(result.rows_written, 40);
    assert_eq!(receipt.generation, 1);
    assert!(std::fs::metadata(summary).unwrap().len() > 0);
    assert_eq!(
        before,
        std::fs::read(fixture.metadata_location().strip_prefix("file://").unwrap()).unwrap()
    );
}

#[tokio::test]
async fn bad_plan_hash_is_refused_before_creating_a_receipt_directory() {
    let (_fixture, mut request) = setup().await;
    request.plan_sha256 = "0".repeat(64);
    let directory = request.receipt_dir.clone();
    assert!(matches!(
        local::run(request).await,
        Err(LocalError::Plan(_))
    ));
    assert!(!directory.exists());
}

#[tokio::test]
async fn previous_receipts_are_never_overwritten() {
    let (_fixture, request) = setup().await;
    std::fs::create_dir(&request.receipt_dir).unwrap();
    let sentinel = request.receipt_dir.join("0.pb");
    std::fs::write(&sentinel, b"existing receipt").unwrap();
    assert!(matches!(local::run(request).await, Err(LocalError::Io(_))));
    assert_eq!(std::fs::read(sentinel).unwrap(), b"existing receipt");
}

#[tokio::test]
async fn configured_resource_failure_never_persists_a_successful_receipt() {
    let (fixture, mut request) = setup().await;
    let limits = fixture.root().join("limits.json");
    std::fs::write(&limits, br#"{"batch_memory_bytes":1}"#).unwrap();
    request.limits_file = Some(limits);
    let directory = request.receipt_dir.clone();
    assert!(matches!(
        local::run(request).await,
        Err(LocalError::Execution(_))
    ));
    assert!(!directory.join("0.pb").exists());
    assert!(!directory.join("summary.json").exists());
}

#[tokio::test]
async fn missing_inputs_and_malformed_limits_are_reported() {
    let (_fixture, mut request) = setup().await;
    request.limits_file = Some(PathBuf::from("/nonexistent/bergsmith-limits.json"));
    assert!(matches!(local::run(request).await, Err(LocalError::Io(_))));
    let (fixture, mut request) = setup().await;
    let limits = fixture.root().join("limits.json");
    std::fs::write(&limits, br#"{"unknown":1}"#).unwrap();
    request.limits_file = Some(limits);
    assert!(matches!(
        local::run(request).await,
        Err(LocalError::Json(_))
    ));
}

async fn write_plan(
    fixture: &bergsmith_fixtures::Fixture,
    metadata: &iceberg::spec::TableMetadata,
    metadata_location: &str,
    config: PlannerConfig,
) -> LocalRequest {
    let PlanOutcome::Planned(plan) = Planner::new(config)
        .unwrap()
        .plan(
            PlanInput {
                metadata,
                metadata_location: Some(metadata_location),
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
    let plan_file = fixture.root().join("plan.json");
    std::fs::write(&plan_file, plan.to_json().unwrap()).unwrap();
    LocalRequest {
        plan_file,
        plan_sha256: plan.sha256().unwrap().to_hex(),
        receipt_dir: fixture.root().join("receipts"),
        limits_file: None,
    }
}

#[tokio::test]
async fn every_task_of_a_multi_task_plan_gets_its_own_attempt_and_receipt() {
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
    let data = |tag: &'static str, rows: i64| {
        FileSpec::data(tag, (0..rows).map(move |id| row![1 => id, 2 => tag]))
            .partition([Value::string(tag)])
    };
    let fixture = TableSpec::new(schema)
        .partitioned_by(
            UnboundPartitionSpec::builder()
                .add_partition_field(2, "tag", Transform::Identity)
                .unwrap()
                .build(),
        )
        .commit(Commit::new().file(data("x", 3)).file(data("y", 5)))
        .generate()
        .await
        .unwrap();
    let config = PlannerConfig {
        min_input_files: 1,
        min_files_removed: 0,
        ..PlannerConfig::default()
    };
    let request = write_plan(
        &fixture,
        fixture.metadata(),
        fixture.metadata_location(),
        config,
    )
    .await;
    let run = local::run(request).await.unwrap();
    assert_eq!(run.receipts.len(), 2);
    let mut rows: Vec<_> = run.receipts.iter().map(|r| r.rows_written).collect();
    rows.sort_unstable();
    assert_eq!(rows, [3, 5]);
    assert_ne!(run.receipts[0].attempt_id, run.receipts[1].attempt_id);
    assert_ne!(run.receipts[0].receipt_file, run.receipts[1].receipt_file);
    for result in &run.receipts {
        let receipt =
            TaskReceipt::decode(std::fs::read(&result.receipt_file).unwrap().as_slice()).unwrap();
        assert_eq!(receipt.attempt_id, result.attempt_id);
        assert_eq!(receipt_digest(&receipt).to_hex(), result.receipt_sha256);
    }
}

#[tokio::test]
async fn plans_pinned_outside_the_local_filesystem_are_refused() {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let request = write_plan(
        &fixture,
        fixture.metadata(),
        "s3://bucket/table/metadata/v1.metadata.json",
        PlannerConfig::default(),
    )
    .await;
    let directory = request.receipt_dir.clone();
    let error = local::run(request).await.err().unwrap();
    assert!(matches!(error, LocalError::NonLocal), "{error}");
    assert_eq!(error.reason_code(), "NonLocalStorage");
    assert_eq!(error.disposition(), FailureDisposition::PermanentFailure);
    assert!(!directory.exists());
}

#[tokio::test]
async fn pinned_metadata_that_differs_from_the_plan_baseline_is_refused() {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    // Plan from metadata with a different data root, claiming the fixture's
    // metadata file: the pinned file's baseline then differs from the plan.
    let changed = TableMetadataBuilder::new_from_metadata(fixture.metadata().clone(), None)
        .set_properties(HashMap::from([(
            "write.data.path".to_owned(),
            format!("{}/moved", fixture.metadata().location()),
        )]))
        .unwrap()
        .build()
        .unwrap()
        .metadata;
    let request = write_plan(
        &fixture,
        &changed,
        fixture.metadata_location(),
        PlannerConfig::default(),
    )
    .await;
    let directory = request.receipt_dir.clone();
    let error = local::run(request).await.err().unwrap();
    assert!(matches!(error, LocalError::BaselineMismatch), "{error}");
    assert_eq!(error.reason_code(), "PlanningBaselineMismatch");
    assert_eq!(error.disposition(), FailureDisposition::ReplanRequired);
    assert!(!directory.exists());
}

#[tokio::test]
async fn oversized_pinned_metadata_is_refused_before_it_is_read() {
    let (fixture, mut request) = setup().await;
    let limits = fixture.root().join("limits.json");
    std::fs::write(&limits, br#"{"read_request_bytes":16}"#).unwrap();
    request.limits_file = Some(limits);
    let error = local::run(request).await.err().unwrap();
    assert!(
        matches!(
            error,
            LocalError::Execution(ExecutorError::ResourceLimit(ResourceLimit::ReadBuffer))
        ),
        "{error}"
    );
    assert_eq!(error.reason_code(), "ReadBufferLimitExceeded");
    assert_eq!(error.disposition(), FailureDisposition::PermanentFailure);
}
