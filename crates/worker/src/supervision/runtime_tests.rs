use bergsmith_protocol::cluster::cluster_service_server::{ClusterService, ClusterServiceServer};
use std::sync::atomic::AtomicUsize;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status};

use super::*;

#[test]
fn exits_without_a_report_are_retried_unless_cancellation_or_resource_limits_explain_them() {
    use std::process::ExitStatus;
    assert_eq!(
        exit_reason(ExitStatus::from_raw(0), false, false),
        "TaskSubprocessCrashed"
    );
    assert_eq!(
        exit_reason(ExitStatus::from_raw(42 << 8), false, false),
        "TaskSubprocessCrashed"
    );
    assert_eq!(
        exit_reason(ExitStatus::from_raw(9), true, false),
        "ProcessMemoryLimitExceeded"
    );
    assert_eq!(
        exit_reason(
            ExitStatus::from_raw(rustix::process::Signal::XCPU.as_raw()),
            false,
            false
        ),
        "ProcessCpuLimitExceeded"
    );
    assert_eq!(
        exit_reason(
            ExitStatus::from_raw(rustix::process::Signal::ABORT.as_raw()),
            false,
            false
        ),
        "TaskSubprocessCrashed"
    );
    assert_eq!(
        exit_reason(ExitStatus::from_raw(9), false, true),
        "TaskCancelled"
    );
}

#[test]
fn reporting_retries_transient_and_identity_errors_but_counts_record_rejections() {
    for code in [
        tonic::Code::Unavailable,
        tonic::Code::DeadlineExceeded,
        tonic::Code::Unauthenticated,
        tonic::Code::PermissionDenied,
    ] {
        assert_eq!(next_step(&Err(Status::new(code, "x").into())), Next::Retry);
    }
    for code in [
        tonic::Code::FailedPrecondition,
        tonic::Code::NotFound,
        tonic::Code::InvalidArgument,
        tonic::Code::Internal,
    ] {
        assert_eq!(
            next_step(&Err(Status::new(code, "x").into())),
            Next::Rejected("ReportRejected")
        );
    }
    assert_eq!(next_step(&Ok(())), Next::Acknowledged);
    assert_eq!(
        next_step(&Err(std::io::Error::other("disk").into())),
        Next::Retry
    );
    assert_eq!(
        next_step(&Err(WorkerError::Invalid("corrupt receipt digest"))),
        Next::Rejected("InvalidJournalRecord")
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn a_transient_memory_sample_failure_is_tolerated_and_persistent_ones_retry() {
    let mut failures = 0;
    assert!(memory_verdict(None, 10, true, &mut failures).is_none());
    assert!(memory_verdict(None, 10, true, &mut failures).is_none());
    // A successful sample resets the run of failures.
    assert!(memory_verdict(Some(5), 10, true, &mut failures).is_none());
    assert_eq!(failures, 0);
    assert!(memory_verdict(None, 10, false, &mut failures).is_none());
    for _ in 1..MAX_MEMORY_SAMPLE_FAILURES {
        assert!(memory_verdict(None, 10, true, &mut failures).is_none());
    }
    let unavailable = memory_verdict(None, 10, true, &mut failures).unwrap();
    assert_eq!(unavailable.reason_code, "ProcessMemoryUsageUnavailable");
    assert_eq!(
        unavailable.disposition,
        i32::from(common::FailureDisposition::RetryTask)
    );
    let exceeded = memory_verdict(Some(11), 10, true, &mut failures).unwrap();
    assert_eq!(exceeded.reason_code, "ProcessMemoryLimitExceeded");
    assert_eq!(
        exceeded.disposition,
        i32::from(common::FailureDisposition::PermanentFailure)
    );
}

#[derive(Clone, Default)]
struct Coordinator {
    polls: Arc<Mutex<Vec<String>>>,
    reports: Arc<Mutex<Vec<cluster::ReportTaskResultRequest>>>,
    heartbeat_count: Arc<AtomicUsize>,
    second_poll: Arc<tokio::sync::Notify>,
    reported: Arc<tokio::sync::Notify>,
    heartbeat_seen: Arc<tokio::sync::Notify>,
    heartbeat_response: Arc<Mutex<cluster::HeartbeatResponse>>,
    failure_stale: Arc<AtomicBool>,
    attempt_status: Arc<Mutex<cluster::GetAttemptStatusResponse>>,
    poll_session_lost: Arc<AtomicBool>,
    registrations: Arc<AtomicUsize>,
    registered_again: Arc<tokio::sync::Notify>,
    result_rejected: Arc<AtomicBool>,
    result_errors: Arc<Mutex<std::collections::VecDeque<tonic::Code>>>,
    grant: Arc<Mutex<Option<cluster::TaskGrant>>>,
    failures: Arc<Mutex<Vec<cluster::ReportTaskFailureRequest>>>,
}

#[tonic::async_trait]
impl ClusterService for Coordinator {
    async fn register_worker(
        &self,
        request: Request<cluster::RegisterWorkerRequest>,
    ) -> Result<Response<cluster::RegisterWorkerResponse>, Status> {
        if self.registrations.fetch_add(1, Ordering::AcqRel) > 0 {
            self.registered_again.notify_one();
        }
        assert_eq!(
            ServiceToken::from_metadata(request.metadata())
                .unwrap()
                .pool(),
            "pool"
        );
        Ok(Response::new(cluster::RegisterWorkerResponse {
            worker_session_id: uuid::Uuid::from_u128(10).to_string(),
            protocol_revision: 2,
            heartbeat_interval: Some(prost_types::Duration {
                seconds: 0,
                nanos: 10_000_000,
            }),
            long_poll_timeout: Some(prost_types::Duration {
                seconds: 30,
                nanos: 0,
            }),
        }))
    }
    async fn request_task(
        &self,
        request: Request<cluster::RequestTaskRequest>,
    ) -> Result<Response<cluster::RequestTaskResponse>, Status> {
        if self.poll_session_lost.swap(false, Ordering::AcqRel) {
            return Err(Status::failed_precondition("worker session expired"));
        }
        if let Some(grant) = self.grant.lock().await.take() {
            return Ok(Response::new(cluster::RequestTaskResponse {
                result: Some(cluster::request_task_response::Result::Grant(grant)),
            }));
        }
        let mut polls = self.polls.lock().await;
        polls.push(request.into_inner().request_id);
        if polls.len() == 1 {
            return Err(Status::unavailable("lost poll response"));
        }
        assert_eq!(polls[0], polls[1]);
        self.second_poll.notify_one();
        drop(polls);
        std::future::pending().await
    }
    async fn heartbeat(
        &self,
        _: Request<cluster::HeartbeatRequest>,
    ) -> Result<Response<cluster::HeartbeatResponse>, Status> {
        self.heartbeat_count.fetch_add(1, Ordering::AcqRel);
        self.heartbeat_seen.notify_one();
        Ok(Response::new(self.heartbeat_response.lock().await.clone()))
    }
    async fn report_task_result(
        &self,
        request: Request<cluster::ReportTaskResultRequest>,
    ) -> Result<Response<cluster::ReportTaskResultResponse>, Status> {
        if self.result_rejected.load(Ordering::Acquire) {
            return Err(Status::failed_precondition("invalid stored plan"));
        }
        if let Some(code) = self.result_errors.lock().await.pop_front() {
            return Err(Status::new(code, "scripted reporting error"));
        }
        let mut reports = self.reports.lock().await;
        reports.push(request.into_inner());
        if reports.len() == 1 {
            return Err(Status::unavailable("lost result acknowledgment"));
        }
        assert_eq!(reports[0], reports[1]);
        self.reported.notify_one();
        Ok(Response::new(cluster::ReportTaskResultResponse {
            outcome: cluster::ResultOutcome::AlreadyAccepted.into(),
            job_state: 4,
        }))
    }
    async fn report_task_failure(
        &self,
        request: Request<cluster::ReportTaskFailureRequest>,
    ) -> Result<Response<cluster::ReportTaskFailureResponse>, Status> {
        self.failures.lock().await.push(request.into_inner());
        if self.failure_stale.load(Ordering::Acquire) {
            return Err(Status::failed_precondition("stale attempt"));
        }
        Ok(Response::new(cluster::ReportTaskFailureResponse {
            disposition: 2,
            job_state: 3,
        }))
    }
    async fn get_attempt_status(
        &self,
        _: Request<cluster::GetAttemptStatusRequest>,
    ) -> Result<Response<cluster::GetAttemptStatusResponse>, Status> {
        Ok(Response::new(*self.attempt_status.lock().await))
    }
    async fn drain_worker(
        &self,
        _: Request<cluster::DrainWorkerRequest>,
    ) -> Result<Response<cluster::DrainWorkerResponse>, Status> {
        Ok(Response::new(cluster::DrainWorkerResponse {}))
    }
}

struct Harness {
    config: WorkerConfig,
    coordinator: Coordinator,
    server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    directory: tempfile::TempDir,
}

async fn harness() -> Harness {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let key = directory.path().join("key.pem");
    let cert = directory.path().join("cert.pem");
    let key_copy = key.clone();
    let cert_copy = cert.clone();
    tokio::task::spawn_blocking(move || {
        let result = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-sha256",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-keyout",
            ])
            .arg(key_copy)
            .arg("-out")
            .arg(cert_copy)
            .output()
            .unwrap();
        assert!(result.status.success());
    })
    .await
    .unwrap();
    let certificate = std::fs::read(cert).unwrap();
    let identity = Identity::from_pem(&certificate, std::fs::read(key).unwrap());
    let coordinator = Coordinator::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(
        Server::builder()
            .tls_config(ServerTlsConfig::new().identity(identity))
            .unwrap()
            .add_service(ClusterServiceServer::new(coordinator.clone()))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    let config = WorkerConfig {
        endpoint: format!("https://localhost:{port}"),
        ca_certificate: Some(certificate),
        token: ServiceToken::parse("bsw1_pool_abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMN-0")
            .unwrap(),
        registration: cluster::RegisterWorkerRequest {
            worker_name: "worker".into(),
            pool: "pool".into(),
            protocol_revision: 2,
            capabilities: vec!["native-v2".into()],
            capacity: Some(cluster::Capacity {
                max_concurrent_tasks: 1,
                memory_bytes: 1024 * 1024 * 1024,
                cpu_millis: 1000,
            }),
            build_version: "test".into(),
        },
        state_directory: directory.path().join("worker"),
        binary: std::env::current_exe().unwrap(),
        storage: StorageProfile::LocalFs,
        cgroup_root: Some(directory.path().to_owned()),
        journal_retention: DEFAULT_JOURNAL_RETENTION,
    };
    Harness {
        config,
        coordinator,
        server,
        directory,
    }
}

#[tokio::test]
async fn heartbeats_continue_while_polling_and_lost_poll_responses_keep_the_same_id() {
    let harness = harness().await;
    assert!(harness.directory.path().exists());
    let (stop, stopped) = watch::channel(false);
    let coordinator = harness.coordinator.clone();
    let run = tokio::spawn(run(harness.config, stopped));
    tokio::time::timeout(Duration::from_secs(10), coordinator.second_poll.notified())
        .await
        .unwrap();
    coordinator.heartbeat_seen.notified().await;
    assert!(coordinator.heartbeat_count.load(Ordering::Acquire) > 0);
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    harness.server.abort();
}

#[tokio::test]
async fn a_poll_that_observes_session_expiry_reregisters_without_restarting_the_worker() {
    let harness = harness().await;
    harness
        .coordinator
        .poll_session_lost
        .store(true, Ordering::Release);
    let coordinator = harness.coordinator.clone();
    let (stop, stopped) = watch::channel(false);
    let run = tokio::spawn(run(harness.config, stopped));
    tokio::time::timeout(
        Duration::from_secs(5),
        coordinator.registered_again.notified(),
    )
    .await
    .unwrap();
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    harness.server.abort();
}

#[tokio::test]
async fn restart_replays_the_original_receipt_and_session_after_a_lost_acknowledgment() {
    let harness = harness().await;
    journal::private_dir(&harness.config.state_directory).unwrap();
    let attempt = harness
        .config
        .state_directory
        .join(uuid::Uuid::from_u128(20).to_string());
    journal::private_dir(&attempt).unwrap();
    let receipt = bergsmith_protocol::artifact::TaskReceipt {
        attempt_id: uuid::Uuid::from_u128(20).to_string(),
        generation: 1,
        ..Default::default()
    };
    let report = cluster::ReportTaskResultRequest {
        worker_session_id: "original-session".into(),
        receipt_sha256: bergsmith_protocol::hashing::receipt_digest(&receipt)
            .as_bytes()
            .to_vec(),
        receipt: Some(receipt),
    };
    journal::persist(&attempt.join("result.pb"), &report).unwrap();
    let (stop, stopped) = watch::channel(false);
    let coordinator = harness.coordinator.clone();
    let run = tokio::spawn(run(harness.config, stopped));
    tokio::time::timeout(Duration::from_secs(10), coordinator.reported.notified())
        .await
        .unwrap();
    // Report persistence follows the RPC completion in the same polling turn.
    tokio::time::timeout(Duration::from_secs(3), async {
        while !attempt.join("ack.pb").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(coordinator.reports.lock().await[1], report);
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    harness.server.abort();
}

fn persist_receipt(state: &Path, attempt: u128) -> PathBuf {
    journal::private_dir(state).unwrap();
    let directory = state.join(uuid::Uuid::from_u128(attempt).to_string());
    journal::private_dir(&directory).unwrap();
    let receipt = bergsmith_protocol::artifact::TaskReceipt {
        attempt_id: uuid::Uuid::from_u128(attempt).to_string(),
        generation: 1,
        ..Default::default()
    };
    let report = cluster::ReportTaskResultRequest {
        worker_session_id: uuid::Uuid::from_u128(10).to_string(),
        receipt_sha256: bergsmith_protocol::hashing::receipt_digest(&receipt)
            .as_bytes()
            .to_vec(),
        receipt: Some(receipt),
    };
    journal::persist(&directory.join("result.pb"), &report).unwrap();
    directory
}

#[tokio::test]
async fn a_report_the_coordinator_keeps_rejecting_is_quarantined_and_polling_resumes() {
    let harness = harness().await;
    harness
        .coordinator
        .result_rejected
        .store(true, Ordering::Release);
    let directory = persist_receipt(&harness.config.state_directory, 21);
    let (stop, stopped) = watch::channel(false);
    let coordinator = harness.coordinator.clone();
    let run = tokio::spawn(run(harness.config, stopped));
    // Polling waits for reporting, so a second poll proves the record settled.
    tokio::time::timeout(Duration::from_secs(10), coordinator.second_poll.notified())
        .await
        .unwrap();
    assert!(directory.join("quarantine.pb").exists());
    assert!(!directory.join("ack.pb").exists());
    let quarantine = common::Failure::decode(
        std::fs::read(directory.join("quarantine.pb"))
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    assert_eq!(quarantine.reason_code, "ReportRejected");
    assert!(Journal::open(&directory.with_file_name("")).is_ok());
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    harness.server.abort();
}

#[tokio::test]
async fn a_transient_report_error_resets_the_consecutive_rejection_count() {
    let harness = harness().await;
    *harness.coordinator.result_errors.lock().await = [
        tonic::Code::FailedPrecondition,
        tonic::Code::Unavailable,
        tonic::Code::FailedPrecondition,
        tonic::Code::FailedPrecondition,
    ]
    .into();
    let directory = persist_receipt(&harness.config.state_directory, 23);
    let (stop, stopped) = watch::channel(false);
    let coordinator = harness.coordinator.clone();
    let run = tokio::spawn(run(harness.config, stopped));
    // The old loop quarantines on the third total rejection, before the
    // coordinator can recover and acknowledge the immutable report.
    tokio::time::timeout(Duration::from_secs(30), coordinator.second_poll.notified())
        .await
        .unwrap();
    assert!(directory.join("ack.pb").exists());
    assert!(!directory.join("quarantine.pb").exists());
    assert_eq!(coordinator.reports.lock().await.len(), 2);
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    harness.server.abort();
}

#[tokio::test]
async fn a_rejected_report_for_an_ended_attempt_is_acknowledged_from_attempt_status() {
    let harness = harness().await;
    harness
        .coordinator
        .result_rejected
        .store(true, Ordering::Release);
    *harness.coordinator.attempt_status.lock().await = cluster::GetAttemptStatusResponse {
        attempt_state: common::AttemptState::Expired.into(),
        result_outcome: cluster::ResultOutcome::StaleAttempt.into(),
        job_state: 3,
    };
    let directory = persist_receipt(&harness.config.state_directory, 22);
    let (stop, stopped) = watch::channel(false);
    let coordinator = harness.coordinator.clone();
    let run = tokio::spawn(run(harness.config, stopped));
    tokio::time::timeout(Duration::from_secs(10), coordinator.second_poll.notified())
        .await
        .unwrap();
    let ack = cluster::GetAttemptStatusResponse::decode(
        std::fs::read(directory.join("ack.pb")).unwrap().as_slice(),
    )
    .unwrap();
    assert_eq!(
        ack.result_outcome,
        i32::from(cluster::ResultOutcome::StaleAttempt)
    );
    assert!(!directory.join("quarantine.pb").exists());
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    harness.server.abort();
}

#[tokio::test]
async fn an_invalid_grant_is_reported_as_a_permanent_failure_and_polling_continues() {
    let harness = harness().await;
    let attempt = uuid::Uuid::from_u128(23).to_string();
    *harness.coordinator.grant.lock().await = Some(cluster::TaskGrant {
        attempt_id: attempt.clone(),
        worker_session_id: uuid::Uuid::from_u128(10).to_string(),
        generation: 1,
        // The worker does not advertise this capability.
        capability_profile: "unknown-capability".into(),
        ..Default::default()
    });
    let (stop, stopped) = watch::channel(false);
    let coordinator = harness.coordinator.clone();
    let state = harness.config.state_directory.clone();
    let run = tokio::spawn(run(harness.config, stopped));
    tokio::time::timeout(Duration::from_secs(10), coordinator.second_poll.notified())
        .await
        .unwrap();
    let failures = coordinator.failures.lock().await.clone();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].attempt_id, attempt);
    let failure = failures[0].failure.clone().unwrap();
    assert_eq!(failure.reason_code, "InvalidTaskGrant");
    assert_eq!(
        failure.disposition,
        i32::from(common::FailureDisposition::PermanentFailure)
    );
    assert!(state.join(&attempt).join("ack.pb").exists());
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    harness.server.abort();
}

#[tokio::test]
async fn a_repeated_grant_never_reexecutes_and_an_interrupted_one_reports_retryably() {
    let harness = harness().await;
    let state = &harness.config.state_directory;
    journal::private_dir(state).unwrap();
    let journal = Journal::open(state).unwrap();
    let exists = || WorkerError::Io(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    let grant = |attempt: u128| cluster::TaskGrant {
        attempt_id: uuid::Uuid::from_u128(attempt).to_string(),
        generation: 2,
        ..Default::default()
    };
    // An earlier execution left its directory without a completion record.
    let interrupted = state.join(uuid::Uuid::from_u128(30).to_string());
    journal::private_dir(&interrupted).unwrap();
    reject(&harness.config, &grant(30), "session", &journal, &exists()).unwrap();
    let report = cluster::ReportTaskFailureRequest::decode(
        std::fs::read(interrupted.join("failure.pb"))
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    assert_eq!(report.generation, 2);
    let failure = report.failure.unwrap();
    assert_eq!(failure.reason_code, "WorkerExecutionInterrupted");
    assert_eq!(
        failure.disposition,
        i32::from(common::FailureDisposition::RetryTask)
    );
    // A completed attempt keeps its original report.
    let completed = persist_receipt(state, 31);
    reject(&harness.config, &grant(31), "session", &journal, &exists()).unwrap();
    assert!(!completed.join("failure.pb").exists());
    // An unparseable attempt has no safe journal path and is left to expiry.
    let mut invalid = grant(32);
    invalid.attempt_id = "../escape".into();
    reject(
        &harness.config,
        &invalid,
        "session",
        &journal,
        &WorkerError::InvalidGrant("invalid attempt id"),
    )
    .unwrap();
    assert_eq!(
        journal.pending().unwrap(),
        vec![interrupted.join("failure.pb")]
    );
    harness.server.abort();
}

#[tokio::test]
async fn coordinator_responses_carry_the_date_used_for_lease_conversion() {
    let harness = harness().await;
    let tls = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(
        harness.config.ca_certificate.clone().unwrap(),
    ));
    let mut client = Client::new(
        Endpoint::from_shared(harness.config.endpoint.clone())
            .unwrap()
            .tls_config(tls)
            .unwrap()
            .connect()
            .await
            .unwrap(),
    );
    let response = client
        .heartbeat(
            request(
                &harness.config,
                cluster::HeartbeatRequest::default(),
                RPC_TIMEOUT,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let date = coordinator_time(response.metadata()).unwrap();
    let skew = date
        .duration_since(SystemTime::now())
        .or_else(|error| Ok::<_, ()>(error.duration()))
        .unwrap();
    assert!(skew < Duration::from_secs(5));
    harness.server.abort();
}

#[tokio::test]
async fn heartbeat_pushes_credentials_and_cancellation_and_obeys_drain() {
    let harness = harness().await;
    let attempt_id = uuid::Uuid::from_u128(99).to_string();
    let credentials = cluster::StorageCredentials {
        prefix: "s3://bucket/table".into(),
        properties: [("s3.session-token".into(), "in-memory-only".into())].into(),
        expires_at: None,
    };
    *harness.coordinator.heartbeat_response.lock().await = cluster::HeartbeatResponse {
        drain: true,
        directives: vec![cluster::AttemptDirective {
            attempt_id: attempt_id.clone(),
            refreshed_credentials: Some(credentials.clone()),
            directive: Some(cluster::attempt_directive::Directive::Cancel(cancel(
                "cancelled by coordinator",
            ))),
        }],
    };
    let (control, mut controls) = watch::channel(Control::default());
    let service = LocalService {
        task: Arc::new(Mutex::new(local::GetTaskResponse::default())),
        control,
        progress: Arc::default(),
        completion: Arc::default(),
        directory: harness.directory.path().to_owned(),
        session: "session".into(),
        journal: Journal::open(harness.directory.path()).unwrap(),
    };
    let active = Arc::new(Mutex::new(Some(Active {
        grant: cluster::TaskGrant {
            attempt_id,
            generation: 1,
            ..Default::default()
        },
        service: service.clone(),
        lease: tokio::time::Instant::now(),
    })));
    let drain = Arc::new(AtomicBool::new(false));
    let tls = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(
        harness.config.ca_certificate.clone().unwrap(),
    ));
    let client = Client::new(
        Endpoint::from_shared(harness.config.endpoint.clone())
            .unwrap()
            .tls_config(tls)
            .unwrap()
            .connect()
            .await
            .unwrap(),
    );
    let (_stop, stopped) = watch::channel(false);
    let future = heartbeat(
        &harness.config,
        client,
        "session".into(),
        Duration::from_secs(1),
        active.clone(),
        drain.clone(),
        stopped,
    );
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("heartbeat unexpectedly stopped: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(5), controls.changed()) => { result.unwrap().unwrap(); },
    }
    let _active = active.lock().await;
    assert_eq!(
        service.task.lock().await.credentials,
        Some(credentials.clone())
    );
    assert_eq!(controls.borrow().credentials, Some(credentials));
    assert!(controls.borrow().cancel.is_some());
    assert!(drain.load(Ordering::Acquire));
    harness.server.abort();
}

#[tokio::test]
async fn failure_reporting_resolves_fenced_attempts_without_acknowledging_live_attempts() {
    let harness = harness().await;
    harness
        .coordinator
        .failure_stale
        .store(true, Ordering::Release);
    let directory = harness.directory.path().join("attempt");
    journal::private_dir(&directory).unwrap();
    let path = directory.join("failure.pb");
    journal::persist(
        &path,
        &cluster::ReportTaskFailureRequest {
            worker_session_id: "original-session".into(),
            attempt_id: uuid::Uuid::from_u128(99).to_string(),
            generation: 1,
            failure: Some(failure("TaskCancelled", false)),
        },
    )
    .unwrap();
    let tls = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(
        harness.config.ca_certificate.clone().unwrap(),
    ));
    let mut client = Client::new(
        Endpoint::from_shared(harness.config.endpoint.clone())
            .unwrap()
            .tls_config(tls)
            .unwrap()
            .connect()
            .await
            .unwrap(),
    );
    assert!(
        matches!(report_one(&harness.config, &mut client, &path).await,
        Err(WorkerError::Rpc(error)) if error.code() == tonic::Code::FailedPrecondition)
    );
    assert!(!directory.join("ack.pb").exists());
    // A grant can be stale before the expiry reconciler changes its stored state.
    *harness.coordinator.attempt_status.lock().await = cluster::GetAttemptStatusResponse {
        attempt_state: common::AttemptState::Granted.into(),
        result_outcome: cluster::ResultOutcome::StaleAttempt.into(),
        job_state: 3,
    };
    report_one(&harness.config, &mut client, &path)
        .await
        .unwrap();
    let ack = cluster::GetAttemptStatusResponse::decode(
        std::fs::read(directory.join("ack.pb")).unwrap().as_slice(),
    )
    .unwrap();
    assert_eq!(
        ack.result_outcome,
        i32::from(cluster::ResultOutcome::StaleAttempt)
    );
    harness.server.abort();
}

#[tokio::test]
async fn local_shutdown_persists_a_retryable_interruption_before_the_child_reports_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let (control, _) = watch::channel(Control::default());
    let service = LocalService {
        task: Arc::new(Mutex::new(local::GetTaskResponse {
            attempt_id: uuid::Uuid::from_u128(99).to_string(),
            generation: 1,
            ..Default::default()
        })),
        control,
        progress: Arc::default(),
        completion: Arc::default(),
        directory: directory.path().to_owned(),
        session: "session".into(),
        journal: Journal::open(directory.path()).unwrap(),
    };
    interrupt(&service, "worker shutting down").await.unwrap();
    assert!(service.control.borrow().cancel.is_some());
    service.fail(failure("TaskCancelled", false)).await.unwrap();
    let report = cluster::ReportTaskFailureRequest::decode(
        std::fs::read(directory.path().join("failure.pb"))
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    let failure = report.failure.unwrap();
    assert_eq!(failure.reason_code, "WorkerExecutionInterrupted");
    assert_eq!(
        failure.disposition,
        i32::from(common::FailureDisposition::RetryTask)
    );
}

#[tokio::test]
async fn cli_startup_loads_non_secret_configuration_and_authenticates_from_a_separate_token_file() {
    let harness = harness().await;
    let configuration = harness.directory.path().join("worker.json");
    let token = harness.directory.path().join("token");
    let ca = harness.directory.path().join("ca.pem");
    std::fs::write(
        &token,
        "bsw1_pool_abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMN-0",
    )
    .unwrap();
    std::fs::write(&ca, harness.config.ca_certificate.as_ref().unwrap()).unwrap();
    let config = serde_json::json!({
        "endpoint": harness.config.endpoint, "ca_certificate": ca,
        "worker_name": "worker", "pool": "pool", "capabilities": ["native-v2"],
        "memory_bytes": 1_073_741_824, "cpu_millis": 1000, "state_directory": harness.config.state_directory,
        "cgroup_root": harness.config.cgroup_root, "s3_region": "us-east-1", "s3_path_style": true
    });
    std::fs::write(&configuration, serde_json::to_vec(&config).unwrap()).unwrap();
    let future = crate::supervision::serve(&configuration, &token);
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("worker stopped before polling: {result:?}"),
        () = async { tokio::time::timeout(Duration::from_secs(5), harness.coordinator.second_poll.notified()).await.unwrap(); } => {},
    }
    assert!(harness.config.state_directory.join("worker.lock").exists());
    assert!(harness.coordinator.heartbeat_count.load(Ordering::Acquire) > 0);
    harness.server.abort();
}

#[cfg(not(target_os = "linux"))]
async fn native_grant(
    harness: &Harness,
) -> (
    bergsmith_fixtures::Fixture,
    cluster::TaskGrant,
    LocalService,
    ActiveSlot,
) {
    use bergsmith_common::ids::{AttemptId, JobId, PlanId};
    use bergsmith_maintenance::{PlanIds, PlanInput, PlanOutcome, Planner, PlannerConfig};
    let fixture = bergsmith_fixtures::spike::small_files_with_duplicates_and_nulls()
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
                job_id: JobId::from_uuid(uuid::Uuid::now_v7()),
                plan_id: PlanId::from_uuid(uuid::Uuid::now_v7()),
            },
        )
        .await
        .unwrap()
    else {
        panic!("missing plan");
    };
    let artifact = plan.artifact_of(&plan.tasks()[0]).unwrap();
    let artifact_path = fixture.root().join("artifact.pb");
    std::fs::write(&artifact_path, artifact.encode_to_vec()).unwrap();
    let attempt = AttemptId::from_uuid(uuid::Uuid::now_v7());
    let expiry = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 90;
    let grant = cluster::TaskGrant {
        job_id: artifact.job_id.clone(),
        plan_id: artifact.plan_id.clone(),
        task_id: artifact.task_id.clone(),
        attempt_id: attempt.to_string(),
        generation: 1,
        worker_session_id: "session".into(),
        artifact: Some(cluster::ArtifactRef {
            uri: format!("file://{}", artifact_path.display()),
            sha256: bergsmith_protocol::hashing::artifact_digest(&artifact)
                .as_bytes()
                .to_vec(),
        }),
        metadata_location: fixture.metadata_location().into(),
        max_manifest_entries: 2_000_000,
        input_state_id: plan.sha256().unwrap().to_hex(),
        capability_profile: "native-v2".into(),
        lease_expires_at: Some(prost_types::Timestamp {
            seconds: i64::try_from(expiry).unwrap(),
            nanos: 0,
        }),
        output_prefix: Location::parse(plan.baseline().data_location())
            .unwrap()
            .attempt_output_prefix(plan.job_id(), attempt)
            .to_string(),
        limits: Some(cluster::ResourceLimits {
            memory_bytes: 1024 * 1024 * 1024,
            cpu_millis: 1000,
            max_output_bytes: 1024 * 1024 * 1024,
            max_duration: Some(prost_types::Duration {
                seconds: 30,
                nanos: 0,
            }),
        }),
        credentials: None,
    };
    journal::private_dir(&harness.config.state_directory).unwrap();
    let journal = Journal::open(&harness.config.state_directory).unwrap();
    let (service, expiry) = begin(&harness.config, &grant, "session", journal).unwrap();
    assert_eq!(service.task.lock().await.max_manifest_entries, 2_000_000);
    let active = Arc::new(Mutex::new(Some(Active {
        grant: grant.clone(),
        service: service.clone(),
        lease: local_deadline(expiry, None, SystemTime::now(), tokio::time::Instant::now()),
    })));
    (fixture, grant, service, active)
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn supervisor_durably_reports_a_real_subprocess_crash() {
    use std::os::unix::fs::PermissionsExt;
    let mut harness = harness().await;
    let script = harness.directory.path().join("crash");
    std::fs::write(&script, "#!/bin/sh\nexit 42\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    harness.config.binary = script;
    let (_fixture, grant, service, active) = native_grant(&harness).await;
    let (_stop, stopped) = watch::channel(false);
    supervise(&harness.config, &grant, &service, &active, stopped)
        .await
        .unwrap();
    let report = cluster::ReportTaskFailureRequest::decode(
        std::fs::read(service.directory.join("failure.pb"))
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    assert_eq!(report.failure.unwrap().reason_code, "TaskSubprocessCrashed");
    assert!(!service.directory.join("result.pb").exists());
    harness.server.abort();
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn supervisor_escalates_cancellation_for_a_real_uncooperative_subprocess() {
    use std::os::unix::fs::PermissionsExt;
    let mut harness = harness().await;
    let ready = harness.directory.path().join("ready");
    let script = harness.directory.path().join("stubborn");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ntrap '' TERM\nprintf ready > '{}'\nwhile :; do :; done\n",
            ready.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    harness.config.binary = script;
    let (_fixture, grant, service, active) = native_grant(&harness).await;
    let (_stop, stopped) = watch::channel(false);
    let control = service.control.clone();
    let future = supervise(&harness.config, &grant, &service, &active, stopped);
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("supervisor ended before cancellation: {result:?}"),
        () = async { while !tokio::fs::try_exists(&ready).await.unwrap() { tokio::task::yield_now().await; } } => {},
    }
    control.send_modify(|control| {
        control.cancel = Some(cluster::Cancel {
            reason: "test cancellation".into(),
            grace_period: Some(prost_types::Duration {
                seconds: 0,
                nanos: 1,
            }),
        });
    });
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .unwrap()
        .unwrap();
    let report = cluster::ReportTaskFailureRequest::decode(
        std::fs::read(service.directory.join("failure.pb"))
            .unwrap()
            .as_slice(),
    )
    .unwrap();
    assert_eq!(report.failure.unwrap().reason_code, "TaskCancelled");
    assert!(!service.directory.join("result.pb").exists());
    harness.server.abort();
}

#[tokio::test]
async fn retention_shorter_than_an_hour_is_rejected_before_startup() {
    let mut harness = harness().await;
    harness.config.journal_retention = Duration::from_secs(MIN_JOURNAL_RETENTION.as_secs() - 1);
    let (_stop, stopped) = watch::channel(false);

    let error = Box::pin(run(harness.config, stopped)).await.unwrap_err();

    assert!(matches!(
        error,
        WorkerError::Invalid("journal retention must be at least one hour")
    ));
    harness.server.abort();
}

#[tokio::test]
async fn startup_removes_expired_acknowledged_records_before_recovery() {
    let mut harness = harness().await;
    harness.config.journal_retention = MIN_JOURNAL_RETENTION;
    journal::private_dir(&harness.config.state_directory).unwrap();
    let attempt = harness
        .config
        .state_directory
        .join(uuid::Uuid::from_u128(30).to_string());
    journal::private_dir(&attempt).unwrap();
    for name in ["result.pb", "ack.pb"] {
        let path = attempt.join(name);
        std::fs::write(&path, b"settled").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(
                SystemTime::now()
                    .checked_sub(2 * MIN_JOURNAL_RETENTION)
                    .unwrap(),
            )
            .unwrap();
    }
    let (stop, stopped) = watch::channel(false);
    let run = tokio::spawn(run(harness.config, stopped));

    tokio::time::timeout(Duration::from_secs(5), async {
        while attempt.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    harness.server.abort();
}
