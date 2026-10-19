//! Independent heartbeat, polling/execution, and immutable reporting loops.

use std::os::unix::{fs::DirBuilderExt, process::ExitStatusExt};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime};

use bergsmith_protocol::{artifact::TaskArtifact, cluster, identity::ServiceToken, local};
use bergsmith_storage::{CredentialCell, Location, StorageProfile};
use prost::Message;
use tokio::sync::{Mutex, watch};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

use super::journal::{self, Journal};
use super::service::{Control, LocalService};
use super::{
    MAX_MESSAGE, RPC_TIMEOUT, WorkerError, credentials_expired, deadline, duration, failure,
};

type Client = cluster::cluster_service_client::ClusterServiceClient<Channel>;

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;

/// Configuration for a storage-only worker supervisor.
/// Tokens and vended credentials are never written into its state directory.
#[derive(Clone)]
pub struct WorkerConfig {
    /// HTTPS coordinator endpoint.
    pub endpoint: String,
    /// Optional private CA PEM; normal system roots remain available.
    pub ca_certificate: Option<Vec<u8>>,
    /// Pool-scoped worker identity.
    pub token: ServiceToken,
    /// Worker identity, capabilities and one-task capacity.
    pub registration: cluster::RegisterWorkerRequest,
    /// Private durable journal; exclusively locked by this worker process.
    pub state_directory: PathBuf,
    /// Same-build worker binary used for `exec` subprocesses.
    pub binary: PathBuf,
    /// Non-secret worker-pool storage routing.
    pub storage: StorageProfile,
    /// Linux delegated cgroup v2 directory with memory, cpu and pids controllers.
    pub cgroup_root: Option<PathBuf>,
}

impl WorkerConfig {
    fn validate(&self) -> Result<(), WorkerError> {
        if !self.endpoint.starts_with("https://")
            || self.registration.pool != self.token.pool()
            || self.registration.protocol_revision != 2
            || self.registration.capacity.as_ref().is_none_or(|capacity| {
                capacity.max_concurrent_tasks != 1
                    || capacity.cpu_millis == 0
                    || capacity.memory_bytes == 0
            })
            || self.registration.worker_name.is_empty()
            || self.registration.capabilities.is_empty()
            || self
                .registration
                .capabilities
                .windows(2)
                .any(|values| values[0] >= values[1])
        {
            return Err(WorkerError::Invalid("invalid worker identity or capacity"));
        }
        if !matches!(
            self.storage,
            StorageProfile::LocalFs | StorageProfile::S3(_)
        ) {
            return Err(WorkerError::Invalid("unsupported storage profile"));
        }
        #[cfg(target_os = "linux")]
        if self.cgroup_root.is_none() {
            return Err(WorkerError::Invalid(
                "linux requires a delegated cgroup v2 root",
            ));
        }
        Ok(())
    }
}

fn request<T>(
    config: &WorkerConfig,
    value: T,
    timeout: Duration,
) -> Result<tonic::Request<T>, WorkerError> {
    let mut request = tonic::Request::new(value);
    let mut token = config
        .token
        .authorization_value()
        .parse::<tonic::metadata::MetadataValue<_>>()
        .map_err(|_| WorkerError::Invalid("invalid authorization metadata"))?;
    token.set_sensitive(true);
    request.metadata_mut().insert("authorization", token);
    request.set_timeout(timeout);
    Ok(request)
}

async fn rpc<T>(
    limit: Duration,
    call: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
) -> Result<tonic::Response<T>, tonic::Status> {
    tokio::time::timeout(limit, call)
        .await
        .unwrap_or_else(|_| Err(tonic::Status::deadline_exceeded("worker rpc timed out")))
}

/// Runs the distributed worker until shutdown or authoritative drain.
///
/// Heartbeats continue while polling, executing, and retrying persisted reports.
/// On shutdown active work receives cooperative cancellation followed by signals.
/// Unacknowledged journal entries survive shutdown and are retried on restart.
///
/// # Errors
/// Returns invalid configuration, corrupt journal, local I/O, or transport errors.
/// Transient coordinator RPC failures retain their original poll/report identity.
pub async fn run(config: WorkerConfig, shutdown: watch::Receiver<bool>) -> Result<(), WorkerError> {
    config.validate()?;
    let state = config.state_directory.clone();
    let _lock = tokio::task::spawn_blocking(move || journal::lock(&state))
        .await
        .map_err(|_| WorkerError::Invalid("state lock task failed"))??;
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let mut tls = ClientTlsConfig::new().with_native_roots();
    if let Some(ca) = &config.ca_certificate {
        tls = tls.ca_certificate(Certificate::from_pem(ca));
    }
    let endpoint = Endpoint::from_shared(config.endpoint.clone())?
        .tls_config(tls)?
        .connect_timeout(RPC_TIMEOUT);
    // Lazy connection keeps coordinator outages inside the explicit retry loops.
    let client = Client::new(endpoint.connect_lazy())
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE);
    let state = config.state_directory.clone();
    let journal = super::blocking(move || Journal::open(&state)).await?;
    tokio::select! {
        result = drive(&config, client.clone(), &journal, shutdown.clone()) => result,
        result = reporting(&config, client, journal.clone()) => result,
    }
}

async fn pause(shutdown: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    if *shutdown.borrow() {
        return true;
    }
    tokio::select! { () = tokio::time::sleep(delay) => false, _ = shutdown.changed() => true }
}

async fn drive(
    config: &WorkerConfig,
    mut client: Client,
    journal: &Journal,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), WorkerError> {
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        let registration = rpc(
            RPC_TIMEOUT,
            client.register_worker(request(config, config.registration.clone(), RPC_TIMEOUT)?),
        )
        .await;
        let registration = match registration {
            Ok(response) => response.into_inner(),
            Err(error) if retryable(&error) => {
                if pause(&mut shutdown, Duration::from_secs(1)).await {
                    return Ok(());
                }
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if registration.protocol_revision != 2 {
            return Err(WorkerError::Invalid("incompatible coordinator protocol"));
        }
        let heartbeat_interval = duration(registration.heartbeat_interval)?;
        let poll_timeout = duration(registration.long_poll_timeout)?.saturating_add(RPC_TIMEOUT);
        let active = Arc::new(Mutex::new(None));
        let drain = Arc::new(AtomicBool::new(false));
        let heartbeat = heartbeat(
            config,
            client.clone(),
            registration.worker_session_id.clone(),
            heartbeat_interval,
            active.clone(),
            drain.clone(),
            shutdown.clone(),
        );
        let polling = polling(
            config,
            client.clone(),
            journal,
            (registration.worker_session_id, poll_timeout),
            active.clone(),
            drain,
            shutdown.clone(),
        );
        tokio::pin!(heartbeat, polling);
        let (poll_ended, result) = tokio::select! {
            result = &mut polling => (true, result),
            result = &mut heartbeat => (false, result),
        };
        if poll_ended && result.is_ok() {
            return result;
        }
        // Fence active native execution before opening another worker session.
        let service = active
            .lock()
            .await
            .as_ref()
            .map(|active| active.service.clone());
        if let Some(service) = service {
            interrupt(&service, "worker session lost").await?;
        }
        // Let the supervisor reap the child; polling notices the sticky cancel.
        if !poll_ended {
            let _ = tokio::time::timeout(Duration::from_secs(15), &mut polling).await;
        }
        match result {
            Err(WorkerError::Rpc(error))
                if matches!(
                    error.code(),
                    tonic::Code::Unauthenticated | tonic::Code::FailedPrecondition
                ) => {}
            result => return result,
        }
    }
}

struct Active {
    grant: cluster::TaskGrant,
    service: LocalService,
    lease: SystemTime,
}
type ActiveSlot = Arc<Mutex<Option<Active>>>;

fn cancel(reason: &str) -> cluster::Cancel {
    cluster::Cancel {
        reason: reason.into(),
        grace_period: Some(prost_types::Duration {
            seconds: 3,
            nanos: 0,
        }),
    }
}

async fn interrupt(service: &LocalService, reason: &str) -> Result<(), WorkerError> {
    // Persist the retryable local stop before the child can report cancellation.
    service
        .fail(failure("WorkerExecutionInterrupted", true))
        .await?;
    service
        .control
        .send_modify(|control| control.cancel = Some(cancel(reason)));
    Ok(())
}

async fn heartbeat(
    config: &WorkerConfig,
    mut client: Client,
    session: String,
    interval: Duration,
    active: ActiveSlot,
    drain: Arc<AtomicBool>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), WorkerError> {
    let mut timer = tokio::time::interval(interval);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! { _ = timer.tick() => {}, _ = shutdown.changed() => return Ok(()) }
        let attempts = match active.lock().await.as_ref() {
            Some(active) => vec![cluster::AttemptLease {
                attempt_id: active.grant.attempt_id.clone(),
                generation: active.grant.generation,
                progress: Some(active.service.progress.lock().await.clone()),
            }],
            None => vec![],
        };
        let response = match rpc(
            RPC_TIMEOUT.min(interval),
            client.heartbeat(request(
                config,
                cluster::HeartbeatRequest {
                    worker_session_id: session.clone(),
                    attempts,
                },
                RPC_TIMEOUT.min(interval),
            )?),
        )
        .await
        {
            Ok(response) => response.into_inner(),
            Err(error) if retryable(&error) => continue,
            Err(error) => return Err(error.into()),
        };
        if response.drain {
            drain.store(true, Ordering::Release);
        }
        let mut slot = active.lock().await;
        if let Some(active) = slot.as_mut() {
            for directive in response.directives {
                if directive.attempt_id != active.grant.attempt_id {
                    continue;
                }
                if let Some(credentials) = directive.refreshed_credentials {
                    let mut task = active.service.task.lock().await;
                    task.credentials = Some(credentials.clone());
                    active
                        .service
                        .control
                        .send_modify(|control| control.credentials = Some(credentials));
                }
                match directive.directive {
                    Some(cluster::attempt_directive::Directive::LeaseExpiresAt(expiry)) => {
                        active.lease = deadline(Some(expiry))?;
                    }
                    Some(cluster::attempt_directive::Directive::Cancel(cancel)) => active
                        .service
                        .control
                        .send_modify(|control| control.cancel = Some(cancel)),
                    Some(cluster::attempt_directive::Directive::Fenced(_)) | None => active
                        .service
                        .control
                        .send_modify(|control| control.cancel = Some(cancel("attempt fenced"))),
                }
            }
        }
    }
}

async fn polling(
    config: &WorkerConfig,
    mut client: Client,
    journal: &Journal,
    session: (String, Duration),
    active: ActiveSlot,
    drain: Arc<AtomicBool>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), WorkerError> {
    let (session, poll_timeout) = session;
    loop {
        if *shutdown.borrow() || drain.load(Ordering::Acquire) {
            return Ok(());
        }
        // Reporting backpressure also drains records recovered from earlier sessions.
        if !journal.pending()?.is_empty() {
            if pause(&mut shutdown, Duration::from_millis(200)).await {
                return Ok(());
            }
            continue;
        }
        let request_id = uuid::Uuid::now_v7().to_string();
        let response = loop {
            let poll = rpc(
                poll_timeout,
                client.request_task(request(
                    config,
                    cluster::RequestTaskRequest {
                        worker_session_id: session.clone(),
                        request_id: request_id.clone(),
                    },
                    poll_timeout,
                )?),
            );
            let result =
                tokio::select! { result = poll => result, _ = shutdown.changed() => return Ok(()) };
            match result {
                Ok(response) => break response.into_inner(),
                Err(error) if retryable(&error) => {
                    if pause(&mut shutdown, Duration::from_secs(1)).await {
                        return Ok(());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        };
        match response.result {
            Some(cluster::request_task_response::Result::Grant(grant)) => {
                let begin_config = config.clone();
                let begin_grant = grant.clone();
                let begin_session = session.clone();
                let begin_journal = journal.clone();
                let service = super::blocking(move || {
                    begin(&begin_config, &begin_grant, &begin_session, begin_journal)
                })
                .await?;
                *active.lock().await = Some(Active {
                    lease: deadline(grant.lease_expires_at)?,
                    grant: grant.clone(),
                    service: service.clone(),
                });
                let result = supervise(config, &grant, &service, &active, shutdown.clone()).await;
                if let Err(error) = result {
                    let credentials = service.task.lock().await.credentials.clone();
                    service
                        .fail(super::worker_failure(&error, credentials.as_ref()))
                        .await?;
                    tracing::warn!(attempt_id = %grant.attempt_id, error = %error, "task supervision failed");
                }
                while !tokio::fs::try_exists(service.directory.join("ack.pb")).await?
                    && !*shutdown.borrow()
                {
                    if service.control.borrow().cancel.is_some() {
                        break;
                    }
                    if pause(&mut shutdown, Duration::from_millis(200)).await {
                        break;
                    }
                }
                *active.lock().await = None;
            }
            Some(cluster::request_task_response::Result::NoTask(no_task)) => {
                if pause(&mut shutdown, duration(no_task.retry_after)?).await {
                    return Ok(());
                }
            }
            Some(cluster::request_task_response::Result::Disposition(_)) => {}
            None => return Err(WorkerError::Invalid("missing poll result")),
        }
    }
}

fn begin(
    config: &WorkerConfig,
    grant: &cluster::TaskGrant,
    session: &str,
    journal: Journal,
) -> Result<LocalService, WorkerError> {
    let attempt: bergsmith_common::ids::AttemptId = grant
        .attempt_id
        .parse()
        .map_err(|_| WorkerError::Invalid("invalid attempt id"))?;
    let limits = grant.limits.ok_or(WorkerError::Invalid("missing limits"))?;
    let capacity = config
        .registration
        .capacity
        .as_ref()
        .ok_or(WorkerError::Invalid("missing capacity"))?;
    if grant.worker_session_id != session
        || grant.generation <= 0
        || grant.metadata_location.is_empty()
        || grant.input_state_id.is_empty()
        || !config
            .registration
            .capabilities
            .contains(&grant.capability_profile)
        || limits.memory_bytes == 0
        || limits.memory_bytes > capacity.memory_bytes
        || limits.cpu_millis == 0
        || limits.cpu_millis > capacity.cpu_millis
        || limits.max_output_bytes == 0
    {
        return Err(WorkerError::Invalid("invalid task grant"));
    }
    duration(limits.max_duration)?;
    let directory = config.state_directory.join(attempt.to_string());
    // A repeated grant must recover its report, never execute the same attempt twice.
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    let (region, endpoint, path_style) = match &config.storage {
        StorageProfile::S3(profile) => (
            profile.region.clone(),
            profile.endpoint.clone(),
            profile.path_style_access,
        ),
        _ => (String::new(), None, false),
    };
    let task = local::GetTaskResponse {
        attempt_id: grant.attempt_id.clone(),
        generation: grant.generation,
        artifact: None,
        limits: Some(limits),
        output_prefix: grant.output_prefix.clone(),
        credentials: grant.credentials.clone(),
        metadata_location: grant.metadata_location.clone(),
        max_manifest_entries: grant.max_manifest_entries,
        artifact_sha256: grant
            .artifact
            .as_ref()
            .map_or_else(Vec::new, |artifact| artifact.sha256.clone()),
        s3_region: region,
        s3_endpoint: endpoint,
        s3_path_style: path_style,
    };
    let (control, _) = watch::channel(Control {
        cancel: None,
        credentials: None,
    });
    Ok(LocalService {
        task: Arc::new(Mutex::new(task)),
        control,
        progress: Arc::default(),
        completion: Arc::default(),
        directory,
        session: session.into(),
        journal,
    })
}

async fn prepare(grant: &cluster::TaskGrant, service: &LocalService) -> Result<(), WorkerError> {
    let task = service.task.lock().await.clone();
    let reference = grant
        .artifact
        .as_ref()
        .ok_or(WorkerError::Invalid("missing artifact ref"))?;
    let location = Location::parse(&reference.uri)
        .map_err(|_| WorkerError::Invalid("invalid artifact location"))?;
    let cell = CredentialCell::default();
    let store = super::task::store(
        &location,
        &task.s3_region,
        task.s3_endpoint,
        task.s3_path_style,
        task.credentials.as_ref(),
        &cell,
    )?;
    let input = store
        .file_io()
        .new_input(location.to_string())
        .map_err(bergsmith_maintenance::ExecutorError::from)?;
    let bytes = tokio::time::timeout(RPC_TIMEOUT, async {
        if input.metadata().await?.size > MAX_MESSAGE as u64 {
            return Err(bergsmith_maintenance::ExecutorError::InvalidData(
                "oversized artifact",
            ));
        }
        input
            .read()
            .await
            .map_err(bergsmith_maintenance::ExecutorError::from)
    })
    .await
    .map_err(|_| WorkerError::Timeout)??;
    let artifact = TaskArtifact::decode(bytes)?;
    if reference.sha256 != bergsmith_protocol::hashing::artifact_digest(&artifact).as_bytes()
        || artifact.job_id != grant.job_id
        || artifact.plan_id != grant.plan_id
        || artifact.task_id != grant.task_id
    {
        return Err(WorkerError::Invalid("artifact differs from grant"));
    }
    service.task.lock().await.artifact = Some(artifact);
    Ok(())
}

async fn supervise(
    config: &WorkerConfig,
    grant: &cluster::TaskGrant,
    service: &LocalService,
    active: &ActiveSlot,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), WorkerError> {
    let limits = grant
        .limits
        .as_ref()
        .ok_or(WorkerError::Invalid("missing limits"))?;
    let maximum = tokio::time::Instant::now() + duration(limits.max_duration)?;
    if credentials_expired(service.task.lock().await.credentials.as_ref()) {
        service
            .fail(failure("StorageAuthorizationExpired", true))
            .await?;
        return Ok(());
    }
    tokio::time::timeout_at(maximum, prepare(grant, service))
        .await
        .map_err(|_| {
            bergsmith_maintenance::ExecutorError::ResourceLimit(
                bergsmith_maintenance::ResourceLimit::Duration,
            )
        })??;
    let expired = active
        .lock()
        .await
        .as_ref()
        .is_none_or(|active| active.lease <= SystemTime::now());
    if *shutdown.borrow() || expired {
        interrupt(service, "worker stopped before execution").await?;
        return Ok(());
    }
    if service.control.borrow().cancel.is_some() {
        service.fail(failure("TaskCancelled", false)).await?;
        return Ok(());
    }
    let socket = service.directory.join("task.sock");
    let listener = tokio::net::UnixListener::bind(&socket)?;
    let binary = config.binary.clone();
    let spawn_socket = socket.clone();
    let spawn_limits = *limits;
    let cgroup_root = config.cgroup_root.clone();
    let mut process = super::blocking(move || {
        super::process::Process::spawn(
            &binary,
            &spawn_socket,
            &spawn_limits,
            cgroup_root.as_deref(),
        )
    })
    .await?;
    let local_service = service.clone();
    let server = tonic::transport::Server::builder()
        .add_service(
            local::local_task_service_server::LocalTaskServiceServer::new(local_service)
                .max_decoding_message_size(MAX_MESSAGE)
                .max_encoding_message_size(MAX_MESSAGE),
        )
        .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener));
    tokio::pin!(server);
    let mut timer = tokio::time::interval(Duration::from_millis(100));
    let mut escalation = super::process::Escalation::default();
    loop {
        tokio::select! {
            result = &mut server => { result?; return Err(WorkerError::Invalid("local task server stopped")); }
            status = process.child.wait() => {
                let status = status?;
                if service.completion.lock().await.is_none() {
                    let reason = exit_reason(status, process.resource_failure(), service.control.borrow().cancel.is_some());
                    service.fail(failure(reason, reason == "TaskSubprocessCrashed")).await?;
                }
                break;
            }
            _ = shutdown.changed(), if !*shutdown.borrow() => {
                interrupt(service, "worker shutting down").await?;
            }
            _ = timer.tick() => {
                let now = tokio::time::Instant::now();
                #[cfg(not(target_os = "linux"))]
                check_memory(&mut process, service, limits.memory_bytes).await?;
                let shutting_down = *shutdown.borrow();
                enforce_control(service, active, shutting_down, maximum).await?;
                let cancellation = service.control.borrow().cancel.clone();
                if let Some(cancel) = cancellation
                    && let Some(signal) = escalation.next_signal(now, cancel.grace_period) {
                    process.signal(signal);
                }
            }
        }
    }
    // Closing the server future immediately closes the UDS; the child is reaped.
    Ok(())
}

fn retryable(error: &tonic::Status) -> bool {
    matches!(
        error.code(),
        tonic::Code::Unavailable
            | tonic::Code::DeadlineExceeded
            | tonic::Code::Cancelled
            | tonic::Code::Unknown
            | tonic::Code::ResourceExhausted
    )
}

async fn enforce_control(
    service: &LocalService,
    active: &ActiveSlot,
    shutdown: bool,
    maximum: tokio::time::Instant,
) -> Result<(), WorkerError> {
    let expired_lease = active
        .lock()
        .await
        .as_ref()
        .is_none_or(|active| active.lease <= SystemTime::now());
    let expired_credentials = credentials_expired(service.task.lock().await.credentials.as_ref());
    if expired_credentials {
        service
            .fail(failure("StorageAuthorizationExpired", true))
            .await?;
        service
            .control
            .send_modify(|control| control.cancel = Some(cancel("storage credentials expired")));
    } else if tokio::time::Instant::now() >= maximum {
        service.fail(failure("TaskDurationExceeded", false)).await?;
        service
            .control
            .send_modify(|control| control.cancel = Some(cancel("task duration exceeded")));
    } else if expired_lease || shutdown {
        interrupt(service, "attempt lease ended or worker stopped").await?;
    }
    Ok(())
}

fn exit_reason(
    status: std::process::ExitStatus,
    memory_limit: bool,
    cancelled: bool,
) -> &'static str {
    if memory_limit {
        "ProcessMemoryLimitExceeded"
    } else if status.signal() == Some(rustix::process::Signal::XCPU.as_raw()) {
        "ProcessCpuLimitExceeded"
    } else if cancelled {
        "TaskCancelled"
    } else {
        "TaskSubprocessCrashed"
    }
}

#[cfg(not(target_os = "linux"))]
async fn check_memory(
    process: &mut super::process::Process,
    service: &LocalService,
    limit: u64,
) -> Result<(), WorkerError> {
    if let Some(pid) = process.child.id() {
        let memory = super::blocking(move || super::process::resident_memory(pid)).await;
        let reason = if memory.as_ref().is_ok_and(|memory| *memory > limit) {
            Some("ProcessMemoryLimitExceeded")
        } else if memory.is_err() && process.child.try_wait()?.is_none() {
            Some("ProcessMemoryUsageUnavailable")
        } else {
            None
        };
        if let Some(reason) = reason {
            service.fail(failure(reason, false)).await?;
            process.signal(rustix::process::Signal::KILL);
        }
    }
    Ok(())
}

async fn reporting(
    config: &WorkerConfig,
    mut client: Client,
    journal: Journal,
) -> Result<(), WorkerError> {
    let mut delay = Duration::from_millis(200);
    loop {
        let mut retry = false;
        for path in journal.pending()? {
            let result = report_one(config, &mut client, &path).await;
            match result {
                Ok(()) => journal.acknowledged(&path)?,
                Err(WorkerError::Rpc(error)) if retryable(&error) => {
                    retry = true;
                }
                Err(error) => return Err(error),
            }
        }
        tokio::time::sleep(delay).await;
        delay = if retry {
            (delay * 2).min(Duration::from_secs(30))
        } else {
            Duration::from_millis(200)
        };
    }
}

async fn report_one(
    config: &WorkerConfig,
    client: &mut Client,
    path: &Path,
) -> Result<(), WorkerError> {
    let ack = path.with_file_name("ack.pb");
    if path.file_name().is_some_and(|name| name == "result.pb") {
        let report_path = path.to_owned();
        let report = super::blocking(move || Journal::result(&report_path)).await?;
        let response = rpc(
            RPC_TIMEOUT,
            client.report_task_result(request(config, report, RPC_TIMEOUT)?),
        )
        .await?
        .into_inner();
        if cluster::ResultOutcome::try_from(response.outcome)
            .ok()
            .is_none_or(|value| value == cluster::ResultOutcome::Unspecified)
        {
            return Err(WorkerError::Invalid("unknown result disposition"));
        }
        super::blocking(move || journal::persist(&ack, &response)).await?;
    } else {
        let bytes = tokio::fs::read(path).await?;
        if bytes.len() > MAX_MESSAGE {
            return Err(WorkerError::Invalid("oversized failure record"));
        }
        let report = cluster::ReportTaskFailureRequest::decode(bytes.as_slice())?;
        let query = cluster::GetAttemptStatusRequest {
            worker_session_id: report.worker_session_id.clone(),
            attempt_id: report.attempt_id.clone(),
        };
        let response = match rpc(
            RPC_TIMEOUT,
            client.report_task_failure(request(config, report, RPC_TIMEOUT)?),
        )
        .await
        {
            Ok(response) => response.into_inner(),
            Err(error) if error.code() == tonic::Code::FailedPrecondition => {
                let status = rpc(
                    RPC_TIMEOUT,
                    client.get_attempt_status(request(config, query, RPC_TIMEOUT)?),
                )
                .await?
                .into_inner();
                match cluster::ResultOutcome::try_from(status.result_outcome) {
                    Ok(
                        cluster::ResultOutcome::AlreadyAccepted
                        | cluster::ResultOutcome::StaleAttempt
                        | cluster::ResultOutcome::Cancelled,
                    ) => {
                        super::blocking(move || journal::persist(&ack, &status)).await?;
                        return Ok(());
                    }
                    Ok(cluster::ResultOutcome::Unspecified) => {
                        return Err(tonic::Status::unavailable(
                            "failure report remains unresolved",
                        )
                        .into());
                    }
                    _ => return Err(WorkerError::Invalid("invalid failure reporting status")),
                }
            }
            Err(error) => return Err(error.into()),
        };
        if bergsmith_protocol::common::FailureDisposition::try_from(response.disposition)
            .ok()
            .is_none_or(|value| {
                value == bergsmith_protocol::common::FailureDisposition::Unspecified
            })
        {
            return Err(WorkerError::Invalid("unknown failure disposition"));
        }
        super::blocking(move || journal::persist(&ack, &response)).await?;
    }
    Ok(())
}
