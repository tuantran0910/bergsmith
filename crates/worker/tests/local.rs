//! Local mode runs a hashed plan without database or catalog access.

#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "fixture and assertion helpers fail the test"
)]

use std::path::PathBuf;

use bergsmith_common::ids::{JobId, PlanId};
use bergsmith_fixtures::spike;
use bergsmith_maintenance::{PlanIds, PlanInput, PlanOutcome, Planner, PlannerConfig};
use bergsmith_protocol::artifact::TaskReceipt;
use bergsmith_protocol::hashing::receipt_digest;
use bergsmith_worker::local::{self, LocalError, LocalRequest};
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
