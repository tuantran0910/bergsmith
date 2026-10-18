//! Real same-build task subprocesses over private Unix gRPC.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test setup and assertions fail immediately"
)]

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bergsmith_common::ids::{AttemptId, JobId, PlanId};
use bergsmith_fixtures::spike;
use bergsmith_maintenance::{PlanIds, PlanInput, PlanOutcome, Planner, PlannerConfig};
use bergsmith_protocol::local::local_task_service_server::{
    LocalTaskService, LocalTaskServiceServer,
};
use bergsmith_protocol::{artifact, cluster, local};
use futures::Stream;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use uuid::Uuid;

#[derive(Clone)]
struct Supervisor {
    task: local::GetTaskResponse,
    cancel: bool,
    result: Arc<Mutex<Option<artifact::TaskReceipt>>>,
    failure: Arc<Mutex<Option<String>>>,
}

#[tonic::async_trait]
impl LocalTaskService for Supervisor {
    async fn get_task(
        &self,
        _: Request<local::GetTaskRequest>,
    ) -> Result<Response<local::GetTaskResponse>, Status> {
        Ok(Response::new(self.task.clone()))
    }
    async fn report_progress(
        &self,
        _: Request<tonic::Streaming<local::ReportProgressRequest>>,
    ) -> Result<Response<local::ReportProgressResponse>, Status> {
        Ok(Response::new(local::ReportProgressResponse {}))
    }
    type WatchControlStream =
        Pin<Box<dyn Stream<Item = Result<local::WatchControlResponse, Status>> + Send>>;
    async fn watch_control(
        &self,
        _: Request<local::WatchControlRequest>,
    ) -> Result<Response<Self::WatchControlStream>, Status> {
        use futures::StreamExt;
        let message = local::WatchControlResponse {
            control: if self.cancel {
                Some(local::watch_control_response::Control::Cancel(
                    cluster::Cancel::default(),
                ))
            } else {
                None
            },
        };
        Ok(Response::new(Box::pin(
            futures::stream::once(async move { Ok(message) }).chain(futures::stream::pending()),
        )))
    }
    async fn report_result(
        &self,
        request: Request<local::ReportResultRequest>,
    ) -> Result<Response<local::ReportResultResponse>, Status> {
        *self.result.lock().await = request.into_inner().receipt;
        Ok(Response::new(local::ReportResultResponse {}))
    }
    async fn report_failure(
        &self,
        request: Request<local::ReportFailureRequest>,
    ) -> Result<Response<local::ReportFailureResponse>, Status> {
        *self.failure.lock().await = request
            .into_inner()
            .failure
            .map(|failure| failure.reason_code);
        Ok(Response::new(local::ReportFailureResponse {}))
    }
}

async fn run(cancel: bool, expired: bool) -> Supervisor {
    run_with_bootstrap(cancel, expired, false).await
}

async fn run_with_bootstrap(cancel: bool, expired: bool, bootstrap: bool) -> Supervisor {
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
                job_id: JobId::from_uuid(Uuid::now_v7()),
                plan_id: PlanId::from_uuid(Uuid::now_v7()),
            },
        )
        .await
        .unwrap()
    else {
        panic!("missing plan");
    };
    let attempt = AttemptId::from_uuid(Uuid::now_v7());
    let task = &plan.tasks()[0];
    let prefix = bergsmith_storage::Location::parse(plan.baseline().data_location())
        .unwrap()
        .attempt_output_prefix(plan.job_id(), attempt);
    let supervisor = Supervisor {
        task: local::GetTaskResponse {
            attempt_id: attempt.to_string(),
            generation: 1,
            artifact: Some(plan.artifact_of(task).unwrap()),
            artifact_sha256: task.artifact_sha256().unwrap().as_bytes().to_vec(),
            output_prefix: prefix.to_string(),
            metadata_location: fixture.metadata_location().into(),
            limits: Some(cluster::ResourceLimits {
                memory_bytes: 4 * 1024 * 1024 * 1024,
                cpu_millis: 1000,
                max_duration: Some(prost_types::Duration {
                    seconds: 30,
                    nanos: 0,
                }),
                max_output_bytes: 1024 * 1024 * 1024,
            }),
            credentials: expired.then(|| cluster::StorageCredentials {
                expires_at: Some(prost_types::Timestamp {
                    seconds: 1,
                    nanos: 0,
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        cancel,
        result: Arc::default(),
        failure: Arc::default(),
    };
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let socket = directory.path().join("task.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(LocalTaskServiceServer::new(supervisor.clone()))
            .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener)),
    );
    invoke(&socket, directory.path(), bootstrap).await;
    assert!(
        fixture
            .metadata_location()
            .strip_prefix("file://")
            .is_some_and(|path| std::path::Path::new(path).exists())
    );
    server.abort();
    supervisor
}

async fn invoke(socket: &std::path::Path, directory: &std::path::Path, bootstrap: bool) {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bergsmith-worker"));
    command
        .arg("exec")
        .arg("--socket")
        .arg(socket)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .kill_on_drop(true);
    // This tests bootstrap/re-exec ordering, not kernel cgroup enforcement.
    if bootstrap {
        std::fs::write(directory.join("cgroup.procs"), []).unwrap();
        command.arg("--bootstrap-cgroup").arg(directory);
    }
    let mut child = command.spawn().unwrap();
    let pid = child.id().unwrap();
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    if bootstrap {
        assert_eq!(
            std::fs::read_to_string(directory.join("cgroup.procs")).unwrap(),
            pid.to_string()
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cgroup_bootstrap_reexecutes_the_same_pid_before_native_execution() {
    let supervisor = run_with_bootstrap(false, false, true).await;
    assert!(supervisor.failure.lock().await.is_none());
    assert_eq!(
        supervisor
            .result
            .lock()
            .await
            .as_ref()
            .unwrap()
            .metrics
            .as_ref()
            .unwrap()
            .rows_written,
        40
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn failed_cgroup_attachment_never_starts_the_task_runtime() {
    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_bergsmith-worker"))
        .args([
            "exec",
            "--socket",
            "/nonexistent/bergsmith.sock",
            "--parent-pid",
        ])
        .arg(std::process::id().to_string())
        .args(["--bootstrap-cgroup", "/nonexistent/bergsmith-cgroup"])
        .kill_on_drop(true)
        .status()
        .await
        .unwrap();
    assert!(!status.success());
}

#[tokio::test]
async fn same_build_subprocess_rewrites_real_parquet_without_catalog_authority() {
    let supervisor = run(false, false).await;
    assert!(
        supervisor.failure.lock().await.is_none(),
        "{:?}",
        supervisor.failure.lock().await
    );
    let result = supervisor.result.lock().await;
    let receipt = result.as_ref().unwrap();
    assert_eq!(receipt.metrics.as_ref().unwrap().rows_written, 40);
    assert_eq!(receipt.artifact_sha256, supervisor.task.artifact_sha256);
    assert!(supervisor.failure.lock().await.is_none());
}

#[tokio::test]
async fn cooperative_cancellation_stops_the_real_subprocess_without_a_receipt() {
    let supervisor = run(true, false).await;
    assert!(supervisor.result.lock().await.is_none());
    assert_eq!(
        supervisor.failure.lock().await.as_deref(),
        Some("TaskCancelled")
    );
}

#[tokio::test]
async fn expired_credentials_stop_the_real_subprocess_without_a_receipt() {
    let supervisor = run(false, true).await;
    assert!(supervisor.result.lock().await.is_none());
    assert_eq!(
        supervisor.failure.lock().await.as_deref(),
        Some("StorageAuthorizationExpired")
    );
}

#[tokio::test]
async fn subprocess_refuses_a_missing_supervisor() {
    let status = tokio::process::Command::new(env!("CARGO_BIN_EXE_bergsmith-worker"))
        .args([
            "exec",
            "--socket",
            "/nonexistent/bergsmith.sock",
            "--parent-pid",
            "0",
        ])
        .kill_on_drop(true)
        .status()
        .await
        .unwrap();
    assert!(!status.success());
}
