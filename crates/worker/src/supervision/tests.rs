use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use bergsmith_protocol::local::local_task_service_server::{
    LocalTaskService, LocalTaskServiceServer,
};
use bergsmith_protocol::{artifact, cluster, common, local};
use prost::Message;
use tokio::sync::{Mutex, watch};
use tonic::Request;

use super::{
    journal,
    service::{Control, LocalService},
    *,
};

fn service(directory: &std::path::Path) -> LocalService {
    let (control, _) = watch::channel(Control::default());
    LocalService {
        task: Arc::new(Mutex::new(local::GetTaskResponse {
            attempt_id: uuid::Uuid::from_u128(1).to_string(),
            generation: 1,
            artifact: Some(artifact::TaskArtifact {
                task_id: uuid::Uuid::from_u128(2).to_string(),
                ..Default::default()
            }),
            artifact_sha256: vec![1; 32],
            output_prefix: "file:///tmp/table/attempt".into(),
            ..Default::default()
        })),
        control,
        progress: Arc::default(),
        completion: Arc::default(),
        directory: directory.to_owned(),
        session: uuid::Uuid::from_u128(3).to_string(),
        journal: journal::Journal::open(directory).unwrap(),
    }
}

async fn receipt(service: &LocalService) -> artifact::TaskReceipt {
    let task = service.task.lock().await;
    artifact::TaskReceipt {
        task_id: task.artifact.as_ref().unwrap().task_id.clone(),
        attempt_id: task.attempt_id.clone(),
        generation: task.generation,
        artifact_sha256: task.artifact_sha256.clone(),
        ..Default::default()
    }
}

#[test]
fn timestamps_and_durations_reject_missing_negative_and_out_of_range_values() {
    assert!(duration(None).is_err());
    assert!(deadline(None).is_err());
    for seconds in [-1, 0, 8 * 24 * 3600] {
        assert!(duration(Some(prost_types::Duration { seconds, nanos: 0 })).is_err());
    }
    for nanos in [-1, 1_000_000_000] {
        assert!(duration(Some(prost_types::Duration { seconds: 1, nanos })).is_err());
        assert!(deadline(Some(prost_types::Timestamp { seconds: 1, nanos })).is_err());
    }
    assert_eq!(
        duration(Some(prost_types::Duration {
            seconds: 1,
            nanos: 1
        }))
        .unwrap(),
        Duration::new(1, 1)
    );
    assert_eq!(
        deadline(Some(prost_types::Timestamp {
            seconds: 1,
            nanos: 0
        }))
        .unwrap(),
        UNIX_EPOCH + Duration::from_secs(1)
    );
    assert!(
        deadline(Some(prost_types::Timestamp {
            seconds: -1,
            nanos: 0
        }))
        .is_err()
    );
    assert!(!credentials_expired(None));
    assert!(credentials_expired(Some(&cluster::StorageCredentials {
        expires_at: Some(prost_types::Timestamp {
            seconds: 0,
            nanos: 0
        }),
        ..Default::default()
    })));
}

#[test]
fn credential_expiry_wins_the_race_with_a_native_io_failure() {
    let credentials = cluster::StorageCredentials {
        expires_at: Some(prost_types::Timestamp {
            seconds: 1,
            nanos: 0,
        }),
        ..Default::default()
    };
    let error = WorkerError::Execution(bergsmith_maintenance::ExecutorError::InvalidData(
        "storage operation failed",
    ));
    let expired = worker_failure(&error, Some(&credentials));
    assert_eq!(expired.reason_code, "StorageAuthorizationExpired");
    assert_eq!(
        expired.disposition,
        i32::from(common::FailureDisposition::RetryTask)
    );
    assert_eq!(
        worker_failure(&WorkerError::Timeout, None).reason_code,
        "StorageOperationTimedOut"
    );
}

#[test]
fn local_infrastructure_faults_retry_and_only_an_invalid_grant_is_permanent() {
    use common::FailureDisposition::{PermanentFailure, RetryTask};
    let bind = std::io::Error::from(std::io::ErrorKind::AddrInUse);
    let spawn = std::io::Error::from_raw_os_error(rustix::io::Errno::AGAIN.raw_os_error());
    let cgroup = std::io::Error::from_raw_os_error(rustix::io::Errno::BUSY.raw_os_error());
    let decode = artifact::TaskArtifact::decode(&[0xff_u8][..]).unwrap_err();
    let cases = [
        (WorkerError::Io(bind), "WorkerLocalFailure", RetryTask),
        (WorkerError::Io(spawn), "WorkerLocalFailure", RetryTask),
        (WorkerError::Io(cgroup), "WorkerLocalFailure", RetryTask),
        (
            WorkerError::Invalid("task launcher stopped"),
            "WorkerLocalFailure",
            RetryTask,
        ),
        (WorkerError::Decode(decode), "WorkerLocalFailure", RetryTask),
        (
            WorkerError::UnverifiedArtifact("artifact differs from grant"),
            "TaskArtifactUnverified",
            RetryTask,
        ),
        (
            WorkerError::InvalidGrant("invalid task grant"),
            "InvalidTaskGrant",
            PermanentFailure,
        ),
        (WorkerError::Timeout, "StorageOperationTimedOut", RetryTask),
        (
            WorkerError::Rpc(tonic::Status::unavailable("local socket closed")),
            "TaskSubprocessCrashed",
            RetryTask,
        ),
    ];
    for (error, reason, disposition) in cases {
        let failure = worker_failure(&error, None);
        assert_eq!(failure.reason_code, reason, "{error}");
        assert_eq!(failure.disposition, i32::from(disposition), "{error}");
    }
}

#[test]
fn coordinator_lease_deadlines_ignore_worker_clock_skew() {
    let received = tokio::time::Instant::now();
    let coordinator = UNIX_EPOCH + Duration::from_secs(1_000_000);
    let expiry = coordinator + Duration::from_secs(90);
    // The worker's wall clock runs ten minutes ahead of the coordinator.
    let ahead = coordinator + Duration::from_secs(600);
    assert_eq!(
        local_deadline(expiry, Some(coordinator), ahead, received),
        received + Duration::from_secs(89)
    );
    // ...or behind it: the remaining lease is still measured by the coordinator.
    let behind = coordinator - Duration::from_secs(600);
    assert_eq!(
        local_deadline(expiry, Some(coordinator), behind, received),
        received + Duration::from_secs(89)
    );
    // Without a coordinator time the worker clock is the fallback reference.
    assert_eq!(local_deadline(expiry, None, ahead, received), received);
    assert_eq!(
        local_deadline(expiry, None, coordinator, received),
        received + Duration::from_secs(90)
    );
    assert_eq!(
        local_deadline(coordinator, Some(expiry), ahead, received),
        received
    );
}

#[test]
fn http_dates_parse_only_the_imf_fixdate_form() {
    assert_eq!(
        http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
        Some(UNIX_EPOCH + Duration::from_secs(784_111_777))
    );
    assert_eq!(http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(UNIX_EPOCH));
    assert_eq!(
        http_date("Thu, 29 Feb 2024 23:59:59 GMT"),
        Some(UNIX_EPOCH + Duration::from_secs(1_709_251_199))
    );
    for invalid in [
        "",
        "Sunday, 06-Nov-94 08:49:37 GMT",
        "Sun Nov  6 08:49:37 1994",
        "Sun, 06 Nov 1994 08:49:37 UTC",
        "Sun, 06 Foo 1994 08:49:37 GMT",
        "Sun, 32 Nov 1994 08:49:37 GMT",
        "Sun, 06 Nov 1994 24:49:37 GMT",
        "Sun, 06 Nov 1969 08:49:37 GMT",
        "Sun, +6 Nov 1994 08:49:37 GMT",
        "Sun, 06 Nov 1994 08-49:37 GMT",
    ] {
        assert_eq!(http_date(invalid), None, "{invalid}");
    }
    let mut metadata = tonic::metadata::MetadataMap::new();
    assert_eq!(coordinator_time(&metadata), None);
    metadata.insert("date", "Thu, 01 Jan 1970 00:00:00 GMT".parse().unwrap());
    assert_eq!(coordinator_time(&metadata), Some(UNIX_EPOCH));
}

#[test]
fn journal_is_private_exclusively_locked_and_never_overwrites_a_report() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("worker");
    let lock = journal::lock(&directory).unwrap();
    assert!(journal::lock(&directory).is_err());
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let report = cluster::ReportTaskResultResponse {
        outcome: 1,
        job_state: 3,
    };
    let path = directory.join("ack.pb");
    journal::persist(&path, &report).unwrap();
    journal::persist(&path, &report).unwrap();
    assert!(journal::persist(&path, &cluster::ReportTaskResultResponse::default()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), report.encode_to_vec());
    drop(lock);
    assert!(journal::lock(&directory).is_ok());
    let public = root.path().join("public");
    std::fs::create_dir(&public).unwrap();
    std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(journal::private_dir(&public).is_err());
}

#[tokio::test]
async fn result_is_synced_before_acknowledgment_and_replays_are_identical() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let receipt = receipt(&service).await;
    service
        .report_result(Request::new(local::ReportResultRequest {
            receipt: Some(receipt.clone()),
        }))
        .await
        .unwrap();
    let path = root.path().join("result.pb");
    let report = journal::Journal::result(&path).unwrap();
    assert_eq!(report.receipt.as_ref(), Some(&receipt));
    service
        .report_result(Request::new(local::ReportResultRequest {
            receipt: Some(receipt.clone()),
        }))
        .await
        .unwrap();
    let mut conflicting = receipt;
    conflicting.metrics = Some(artifact::ExecutionMetrics {
        rows_read: 1,
        ..Default::default()
    });
    assert!(
        service
            .report_result(Request::new(local::ReportResultRequest {
                receipt: Some(conflicting)
            }))
            .await
            .is_err()
    );
    assert_eq!(journal::Journal::result(&path).unwrap(), report);
    assert!(!root.path().join("ack.pb").exists());
}

#[test]
fn cancelling_a_result_rpc_cannot_publish_a_competing_failure() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (release, blocked) = std::sync::mpsc::channel();
    let (started, ready) = std::sync::mpsc::channel();
    runtime.spawn_blocking(move || {
        started.send(()).unwrap();
        blocked.recv().unwrap();
    });
    ready.recv().unwrap();
    runtime.block_on(async {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        let receipt = receipt(&service).await;
        let mut call = Box::pin(
            service.report_result(Request::new(local::ReportResultRequest {
                receipt: Some(receipt),
            })),
        );
        std::future::poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(call);
        // The queued writer owns the guard even after its RPC has disappeared.
        assert!(service.completion.try_lock().is_err());
        release.send(()).unwrap();
        service
            .fail(failure("TaskSubprocessCrashed", true))
            .await
            .unwrap();
        assert!(matches!(
            *service.completion.lock().await,
            Some(service::Completion::Result)
        ));
        assert!(root.path().join("result.pb").exists());
        assert!(!root.path().join("failure.pb").exists());
        assert_eq!(
            service.journal.pending().unwrap(),
            vec![root.path().join("result.pb")]
        );
    });
}

#[test]
fn cancelling_a_failure_rpc_keeps_its_original_failure() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (release, blocked) = std::sync::mpsc::channel();
    let (started, ready) = std::sync::mpsc::channel();
    runtime.spawn_blocking(move || {
        started.send(()).unwrap();
        blocked.recv().unwrap();
    });
    ready.recv().unwrap();
    runtime.block_on(async {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        let mut call = Box::pin(service.fail(failure("StorageAuthorizationExpired", true)));
        std::future::poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(call);
        assert!(service.completion.try_lock().is_err());
        release.send(()).unwrap();
        service.fail(failure("TaskCancelled", false)).await.unwrap();
        let report = cluster::ReportTaskFailureRequest::decode(
            std::fs::read(root.path().join("failure.pb"))
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        assert_eq!(
            report.failure.unwrap().reason_code,
            "StorageAuthorizationExpired"
        );
        assert!(!root.path().join("result.pb").exists());
    });
}

#[test]
fn pending_queries_use_the_index_without_traversing_acknowledged_history() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    journal::private_dir(&state).unwrap();
    let journal = journal::Journal::open(&state).unwrap();
    let path = state.join("result.pb");
    let report = cluster::ReportTaskResultResponse {
        outcome: 2,
        job_state: 3,
    };
    journal.record(&path, &report).unwrap();
    assert_eq!(journal.pending().unwrap(), vec![path.clone()]);
    journal::persist(&path.with_file_name("ack.pb"), &report).unwrap();
    journal.acknowledged(&path).unwrap();
    journal.record(&path, &report).unwrap();
    assert!(journal.pending().unwrap().is_empty());
    assert!(path.exists());
    // A fresh scan would now fail. Pending queries must use only the index.
    std::fs::rename(&state, root.path().join("history")).unwrap();
    assert!(journal.pending().unwrap().is_empty());
}

#[test]
fn recovery_rejects_conflicting_unacknowledged_completion_records() {
    let root = tempfile::tempdir().unwrap();
    let attempt = root.path().join("attempt");
    journal::private_dir(&attempt).unwrap();
    journal::persist(
        &attempt.join("result.pb"),
        &cluster::ReportTaskResultRequest::default(),
    )
    .unwrap();
    journal::persist(
        &attempt.join("failure.pb"),
        &cluster::ReportTaskFailureRequest::default(),
    )
    .unwrap();
    assert!(journal::Journal::open(root.path()).is_err());
}

#[tokio::test]
async fn cancellation_failure_and_identity_mismatch_cannot_produce_a_result() {
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let receipt = receipt(&service).await;
    let mut mismatch = receipt.clone();
    mismatch.generation += 1;
    assert!(
        service
            .report_result(Request::new(local::ReportResultRequest {
                receipt: Some(mismatch)
            }))
            .await
            .is_err()
    );
    service
        .control
        .send_modify(|control| control.cancel = Some(cluster::Cancel::default()));
    assert!(
        service
            .report_result(Request::new(local::ReportResultRequest {
                receipt: Some(receipt.clone())
            }))
            .await
            .is_err()
    );
    service.fail(failure("TaskCancelled", false)).await.unwrap();
    service
        .fail(failure("TaskSubprocessCrashed", true))
        .await
        .unwrap();
    assert!(
        service
            .report_result(Request::new(local::ReportResultRequest {
                receipt: Some(receipt)
            }))
            .await
            .is_err()
    );
    let report = cluster::ReportTaskFailureRequest::decode(
        std::fs::read(root.path().join("failure.pb"))
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    assert_eq!(report.failure.unwrap().reason_code, "TaskCancelled");
    assert!(!root.path().join("result.pb").exists());
}

#[tokio::test]
async fn control_stream_starts_without_credentials_and_retains_cancel_and_latest_refresh() {
    use futures::StreamExt;
    let root = tempfile::tempdir().unwrap();
    let service = service(root.path());
    let mut stream = service
        .watch_control(Request::new(local::WatchControlRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(stream.next().await.unwrap().unwrap().control.is_none());
    service.control.send_modify(|control| {
        control.cancel = Some(cluster::Cancel {
            reason: "stop".into(),
            grace_period: None,
        });
    });
    for value in ["first-secret", "latest-secret"] {
        service.control.send_modify(|control| {
            control.credentials = Some(cluster::StorageCredentials {
                prefix: "s3://bucket/table".into(),
                properties: [("s3.session-token".into(), value.into())].into(),
                expires_at: None,
            });
        });
    }
    let Some(local::watch_control_response::Control::RefreshedCredentials(value)) =
        stream.next().await.unwrap().unwrap().control
    else {
        panic!("missing refresh");
    };
    assert_eq!(value.properties["s3.session-token"], "latest-secret");
    assert!(matches!(
        stream.next().await.unwrap().unwrap().control,
        Some(local::watch_control_response::Control::Cancel(_))
    ));
}

#[tokio::test]
async fn actual_unix_rpc_acknowledges_only_a_persisted_receipt() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let service = service(directory.path());
    let receipt = receipt(&service).await;
    let socket = directory.path().join("task.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(LocalTaskServiceServer::new(service.clone()))
            .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener)),
    );
    let mut client = task::connect(&socket).await.unwrap();
    assert_eq!(
        client
            .get_task(local::GetTaskRequest {})
            .await
            .unwrap()
            .into_inner()
            .attempt_id,
        receipt.attempt_id
    );
    let stream = client
        .watch_control(local::WatchControlRequest {})
        .await
        .unwrap()
        .into_inner();
    drop(stream);
    client
        .report_result(local::ReportResultRequest {
            receipt: Some(receipt.clone()),
        })
        .await
        .unwrap();
    assert_eq!(
        journal::Journal::result(&directory.path().join("result.pb"))
            .unwrap()
            .receipt,
        Some(receipt)
    );
    let progress = cluster::AttemptProgress {
        rows_read: 1,
        rows_written: 1,
        bytes_written: 1,
        output_paths: vec!["file:///tmp/outside".into()],
    };
    assert!(
        client
            .report_progress(tokio_stream::iter([local::ReportProgressRequest {
                progress: Some(progress)
            }]))
            .await
            .is_err()
    );
    server.abort();
}

#[tokio::test]
async fn restart_discovers_only_unacknowledged_reports_and_checks_the_digest() {
    let root = tempfile::tempdir().unwrap();
    let attempt = root.path().join("attempt");
    journal::private_dir(&attempt).unwrap();
    let service = service(&attempt);
    let receipt = receipt(&service).await;
    service
        .report_result(Request::new(local::ReportResultRequest {
            receipt: Some(receipt),
        }))
        .await
        .unwrap();
    let journal = journal::Journal::open(root.path()).unwrap();
    assert_eq!(journal.pending().unwrap(), vec![attempt.join("result.pb")]);
    journal::persist(
        &attempt.join("ack.pb"),
        &cluster::ReportTaskResultResponse {
            outcome: 2,
            job_state: 3,
        },
    )
    .unwrap();
    journal.acknowledged(&attempt.join("result.pb")).unwrap();
    assert!(journal.pending().unwrap().is_empty());
    assert!(
        journal::Journal::open(root.path())
            .unwrap()
            .pending()
            .unwrap()
            .is_empty()
    );
    let bad = root.path().join("corrupt.pb");
    journal::persist(
        &bad,
        &cluster::ReportTaskResultRequest {
            receipt: Some(artifact::TaskReceipt::default()),
            receipt_sha256: vec![0; 32],
            worker_session_id: "s".into(),
        },
    )
    .unwrap();
    assert!(journal::Journal::result(&bad).is_err());
}

#[test]
fn refreshed_credentials_update_the_existing_provider_without_persisting_secrets() {
    let cell = bergsmith_storage::CredentialCell::default();
    let prefix = bergsmith_storage::Location::parse("s3://bucket/table").unwrap();
    let wire = cluster::StorageCredentials {
        prefix: prefix.to_string(),
        properties: [
            ("s3.access-key-id".into(), "access".into()),
            ("s3.secret-access-key".into(), "secret".into()),
        ]
        .into(),
        expires_at: Some(prost_types::Timestamp {
            seconds: 4_000_000_000,
            nanos: 0,
        }),
    };
    cell.replace(vec![task::credential(&wire).unwrap()]);
    assert!(cell.select(&prefix, Duration::ZERO).is_ok());
    let expired = cluster::StorageCredentials {
        expires_at: Some(prost_types::Timestamp {
            seconds: 1,
            nanos: 0,
        }),
        ..wire
    };
    cell.replace(vec![task::credential(&expired).unwrap()]);
    assert!(cell.select(&prefix, Duration::ZERO).is_err());
    assert_eq!(
        failure("TaskSubprocessCrashed", true).disposition,
        i32::from(common::FailureDisposition::RetryTask)
    );
}
