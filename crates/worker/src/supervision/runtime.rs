//! Independent heartbeat, polling/execution, and immutable reporting loops.

use std::os::unix::{fs::DirBuilderExt, process::ExitStatusExt};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime};

use bergsmith_protocol::{artifact::TaskArtifact, cluster, common, identity::ServiceToken, local};
use bergsmith_storage::{CredentialCell, Location, StorageProfile};
use prost::Message;
use tokio::sync::{Mutex, watch};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

use super::journal::{self, Journal};
use super::service::{Control, LocalService};
use super::{
    MAX_MESSAGE, RPC_TIMEOUT, WorkerError, coordinator_time, credentials_expired, deadline,
    duration, failure, local_deadline,
};

/// Consecutive definitive rejections before a journal record is quarantined.
const MAX_REPORT_REJECTIONS: u32 = 3;
/// Consecutive failed memory samples tolerated on development platforms.
#[cfg(not(target_os = "linux"))]
const MAX_MEMORY_SAMPLE_FAILURES: u32 = 3;

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
    /// How long an acknowledged journal record is kept before removal, at
    /// least [`MIN_JOURNAL_RETENTION`]. Unacknowledged and quarantined records
    /// are never removed.
    pub journal_retention: Duration,
}

/// Default retention for acknowledged journal records.
pub const DEFAULT_JOURNAL_RETENTION: Duration = Duration::from_secs(30 * 24 * 3600);
/// Shortest accepted retention, so recent history stays available for inspection.
pub const MIN_JOURNAL_RETENTION: Duration = Duration::from_secs(3600);
/// How often a running worker removes expired acknowledged records.
const RETENTION_INTERVAL: Duration = Duration::from_secs(3600);

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
        if self.journal_retention < MIN_JOURNAL_RETENTION {
            return Err(WorkerError::Invalid(
                "journal retention must be at least one hour",
            ));
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
    // No task runs yet, so every empty cgroup left by a crashed run is stale.
    #[cfg(target_os = "linux")]
    if let Some(root) = config.cgroup_root.clone() {
        super::blocking(move || {
            super::process::sweep_stale_cgroups(&root, super::process::process_alive)?;
            Ok(())
        })
        .await?;
    }
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
    // Remove expired history before the one-time recovery scan reads it.
    prune(&config).await?;
    let state = config.state_directory.clone();
    let journal = super::blocking(move || Journal::open(&state)).await?;
    let retention = AbortOnDrop(tokio::spawn(retain(config.clone())));
    let result = tokio::select! {
        result = drive(&config, client.clone(), &journal, shutdown.clone()) => result,
        result = reporting(&config, client, journal.clone()) => result,
    };
    drop(retention);
    result
}

async fn prune(config: &WorkerConfig) -> Result<(), WorkerError> {
    let state = config.state_directory.clone();
    let retention = config.journal_retention;
    let removed =
        super::blocking(move || journal::prune(&state, retention, SystemTime::now())).await?;
    if removed > 0 {
        tracing::info!(removed, "removed expired acknowledged journal records");
    }
    Ok(())
}

/// Periodic retention. A failed pass is logged and retried at the next
/// interval; it never stops execution or reporting.
async fn retain(config: WorkerConfig) {
    let mut tick = tokio::time::interval(RETENTION_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tick.tick().await;
        if let Err(error) = prune(&config).await {
            tracing::warn!(error = %error, "journal retention pass failed");
        }
    }
}

/// Stops a background task when the runtime that owns it returns.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
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
    /// Local monotonic lease deadline, immune to worker wall-clock skew.
    lease: tokio::time::Instant,
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
            Ok(response) => response,
            Err(error) if retryable(&error) => continue,
            Err(error) => return Err(error.into()),
        };
        let received = tokio::time::Instant::now();
        let coordinator_now = coordinator_time(response.metadata());
        let response = response.into_inner();
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
                        active.lease = local_deadline(
                            deadline(Some(expiry))?,
                            coordinator_now,
                            SystemTime::now(),
                            received,
                        );
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
        let (received, coordinator_now, response) = loop {
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
                Ok(response) => {
                    let received = tokio::time::Instant::now();
                    let coordinator_now = coordinator_time(response.metadata());
                    break (received, coordinator_now, response.into_inner());
                }
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
                let begun = super::blocking(move || {
                    begin(&begin_config, &begin_grant, &begin_session, begin_journal)
                })
                .await;
                let (service, expiry) = match begun {
                    Ok(begun) => begun,
                    Err(error) => {
                        // Report the refusal for this attempt and keep polling;
                        // the journal backpressure above waits for its ack.
                        if !refuse(config, &grant, &session, journal, error).await
                            && pause(&mut shutdown, Duration::from_secs(1)).await
                        {
                            return Ok(());
                        }
                        continue;
                    }
                };
                *active.lock().await = Some(Active {
                    lease: local_deadline(expiry, coordinator_now, SystemTime::now(), received),
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
                settle(&service, &mut shutdown).await?;
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
) -> Result<(LocalService, SystemTime), WorkerError> {
    let attempt: bergsmith_common::ids::AttemptId = grant
        .attempt_id
        .parse()
        .map_err(|_| WorkerError::InvalidGrant("invalid attempt id"))?;
    let limits = grant
        .limits
        .ok_or(WorkerError::InvalidGrant("missing limits"))?;
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
        return Err(WorkerError::InvalidGrant("invalid task grant"));
    }
    duration(limits.max_duration).map_err(|_| WorkerError::InvalidGrant("invalid max duration"))?;
    let expiry = deadline(grant.lease_expires_at)
        .map_err(|_| WorkerError::InvalidGrant("invalid lease deadline"))?;
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
    let service = LocalService {
        task: Arc::new(Mutex::new(task)),
        control,
        progress: Arc::default(),
        completion: Arc::default(),
        directory,
        session: session.into(),
        journal,
    };
    Ok((service, expiry))
}

/// Keeps the attempt leased until its record is acknowledged or quarantined,
/// it is cancelled, or the worker stops.
async fn settle(
    service: &LocalService,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), WorkerError> {
    loop {
        let directory = service.directory.clone();
        if *shutdown.borrow()
            || service.control.borrow().cancel.is_some()
            || super::blocking(move || Ok(journal::settled(&directory))).await?
            || pause(shutdown, Duration::from_millis(200)).await
        {
            return Ok(());
        }
    }
}

/// Logs and journals a refused grant off the async runtime. Returns whether
/// the refusal was recorded (or needed no record).
async fn refuse(
    config: &WorkerConfig,
    grant: &cluster::TaskGrant,
    session: &str,
    journal: &Journal,
    error: WorkerError,
) -> bool {
    tracing::warn!(attempt_id = %grant.attempt_id, error = %error, "task grant refused");
    let (config, task, session, journal) = (
        config.clone(),
        grant.clone(),
        session.to_owned(),
        journal.clone(),
    );
    let result = super::blocking(move || reject(&config, &task, &session, &journal, &error)).await;
    if let Err(error) = &result {
        tracing::error!(attempt_id = %grant.attempt_id, error = %error, "grant refusal could not be journaled");
    }
    result.is_ok()
}

/// Journals a failure for a grant that could not begin, so the coordinator
/// learns its disposition instead of waiting for lease expiry.
///
/// An attempt directory that already holds a completion is left untouched:
/// a repeated grant recovers its existing report and never executes again.
/// A directory without one means an earlier local execution was interrupted.
fn reject(
    config: &WorkerConfig,
    grant: &cluster::TaskGrant,
    session: &str,
    journal: &Journal,
    error: &WorkerError,
) -> Result<(), WorkerError> {
    let Ok(attempt) = grant.attempt_id.parse::<bergsmith_common::ids::AttemptId>() else {
        // No safe journal path exists; the coordinator expires the attempt.
        return Ok(());
    };
    let directory = config.state_directory.join(attempt.to_string());
    let failure = match error {
        WorkerError::Io(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if ["result.pb", "failure.pb"]
                .iter()
                .any(|name| directory.join(name).exists())
            {
                return Ok(());
            }
            failure("WorkerExecutionInterrupted", true)
        }
        error => super::worker_failure(error, None),
    };
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => return Err(error.into()),
        _ => {}
    }
    journal.record(
        &directory.join("failure.pb"),
        &cluster::ReportTaskFailureRequest {
            worker_session_id: session.into(),
            attempt_id: grant.attempt_id.clone(),
            generation: grant.generation,
            failure: Some(failure),
        },
    )
}

async fn prepare(grant: &cluster::TaskGrant, service: &LocalService) -> Result<(), WorkerError> {
    let task = service.task.lock().await.clone();
    let reference = grant
        .artifact
        .as_ref()
        .ok_or(WorkerError::InvalidGrant("missing artifact ref"))?;
    let location = Location::parse(&reference.uri)
        .map_err(|_| WorkerError::InvalidGrant("invalid artifact location"))?;
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
    let artifact = TaskArtifact::decode(bytes)
        .map_err(|_| WorkerError::UnverifiedArtifact("undecodable artifact"))?;
    if reference.sha256 != bergsmith_protocol::hashing::artifact_digest(&artifact).as_bytes()
        || artifact.job_id != grant.job_id
        || artifact.plan_id != grant.plan_id
        || artifact.task_id != grant.task_id
    {
        return Err(WorkerError::UnverifiedArtifact(
            "artifact differs from grant",
        ));
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
        .is_none_or(|active| active.lease <= tokio::time::Instant::now());
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
    process.forward_output(&grant.attempt_id);
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
    #[cfg(not(target_os = "linux"))]
    let mut sample_failures = 0;
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
                check_memory(&mut process, service, limits.memory_bytes, &mut sample_failures).await?;
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
        .is_none_or(|active| active.lease <= tokio::time::Instant::now());
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

/// Decides the development-platform memory verdict for one RSS sample.
///
/// A sample above the limit fails permanently. A sample that cannot be read
/// while the task is alive fails, retryably, only after
/// `MAX_MEMORY_SAMPLE_FAILURES` consecutive misses, so one transient platform
/// query failure does not stop the task.
#[cfg(not(target_os = "linux"))]
fn memory_verdict(
    sample: Option<u64>,
    limit: u64,
    alive: bool,
    failures: &mut u32,
) -> Option<common::Failure> {
    match sample {
        Some(memory) => {
            *failures = 0;
            (memory > limit).then(|| failure("ProcessMemoryLimitExceeded", false))
        }
        None if alive => {
            *failures += 1;
            (*failures >= MAX_MEMORY_SAMPLE_FAILURES)
                .then(|| failure("ProcessMemoryUsageUnavailable", true))
        }
        None => None,
    }
}

#[cfg(not(target_os = "linux"))]
async fn check_memory(
    process: &mut super::process::Process,
    service: &LocalService,
    limit: u64,
    failures: &mut u32,
) -> Result<(), WorkerError> {
    if let Some(pid) = process.child.id() {
        let memory = super::blocking(move || super::process::resident_memory(pid))
            .await
            .ok();
        let alive = memory.is_some() || process.child.try_wait()?.is_none();
        if let Some(failure) = memory_verdict(memory, limit, alive, failures) {
            service.fail(failure).await?;
            process.signal(rustix::process::Signal::KILL);
        }
    }
    Ok(())
}

/// What the reporting loop does with a record after one reporting turn.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    Acknowledged,
    /// Transient: keep the record and retry with backoff.
    Retry,
    /// The coordinator, or the record itself, definitively refused it.
    Rejected(&'static str),
}

/// Classifies one report outcome. Identity failures are global rather than
/// specific to a record, so they keep it; registration surfaces them.
fn next_step(result: &Result<(), WorkerError>) -> Next {
    match result {
        Ok(()) => Next::Acknowledged,
        Err(WorkerError::Rpc(error)) if !definitive(error) => Next::Retry,
        Err(WorkerError::Io(_) | WorkerError::Timeout | WorkerError::Transport(_)) => Next::Retry,
        Err(WorkerError::Rpc(_)) => Next::Rejected("ReportRejected"),
        Err(_) => Next::Rejected("InvalidJournalRecord"),
    }
}

/// Whether a coordinator refusal is specific to this record, rather than a
/// transient or worker-identity failure.
fn definitive(error: &tonic::Status) -> bool {
    !retryable(error)
        && !matches!(
            error.code(),
            tonic::Code::Unauthenticated | tonic::Code::PermissionDenied
        )
}

/// Reports journal records for the worker's lifetime; one bad record never
/// stops the worker.
///
/// A record that is definitively refused `MAX_REPORT_REJECTIONS` times in a
/// row, and that attempt status does not resolve, is quarantined durably so
/// polling can resume. Its files remain for inspection.
async fn reporting(
    config: &WorkerConfig,
    mut client: Client,
    journal: Journal,
) -> Result<(), WorkerError> {
    let mut delay = Duration::from_millis(200);
    let mut rejections = std::collections::BTreeMap::<PathBuf, u32>::new();
    loop {
        let mut retry = false;
        for path in journal.pending()? {
            let result = report_one(config, &mut client, &path).await;
            match next_step(&result) {
                Next::Acknowledged => {
                    rejections.remove(&path);
                    journal.acknowledged(&path)?;
                }
                Next::Retry => {
                    rejections.remove(&path);
                    retry = true;
                }
                Next::Rejected(reason) => {
                    retry = true;
                    let count = rejections.entry(path.clone()).or_default();
                    *count += 1;
                    let error = result.err().map(|error| error.to_string());
                    tracing::warn!(path = %path.display(), reason, rejections = *count, error = error.as_deref(), "worker report rejected");
                    if *count >= MAX_REPORT_REJECTIONS {
                        quarantine(&journal, &path, reason, error.unwrap_or_default()).await;
                        rejections.remove(&path);
                    }
                }
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

async fn quarantine(journal: &Journal, path: &Path, reason: &'static str, message: String) {
    let record = common::Failure {
        reason_code: reason.into(),
        disposition: common::FailureDisposition::PermanentFailure.into(),
        message,
    };
    let journal = journal.clone();
    let target = path.to_owned();
    match super::blocking(move || journal.quarantine(&target, &record)).await {
        Ok(()) => {
            tracing::error!(path = %path.display(), reason, "worker report quarantined");
        }
        // The record stays pending and is quarantined on a later rejection.
        Err(error) => {
            tracing::error!(path = %path.display(), error = %error, "worker report could not be quarantined");
        }
    }
}

/// Resolves a definitively rejected report through attempt status.
///
/// Only a conclusive ended disposition acknowledges the record. A live
/// attempt keeps the original rejection, which the caller counts.
async fn resolve(
    config: &WorkerConfig,
    client: &mut Client,
    ack: PathBuf,
    query: cluster::GetAttemptStatusRequest,
    rejection: tonic::Status,
) -> Result<(), WorkerError> {
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
            Ok(())
        }
        Ok(cluster::ResultOutcome::Unspecified) => Err(rejection.into()),
        _ => Err(WorkerError::Invalid("invalid attempt status")),
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
        let query = cluster::GetAttemptStatusRequest {
            worker_session_id: report.worker_session_id.clone(),
            attempt_id: report
                .receipt
                .as_ref()
                .map(|receipt| receipt.attempt_id.clone())
                .unwrap_or_default(),
        };
        let response = match rpc(
            RPC_TIMEOUT,
            client.report_task_result(request(config, report, RPC_TIMEOUT)?),
        )
        .await
        {
            Ok(response) => response.into_inner(),
            Err(error) if definitive(&error) => {
                return resolve(config, client, ack, query, error).await;
            }
            Err(error) => return Err(error.into()),
        };
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
            Err(error) if definitive(&error) => {
                return resolve(config, client, ack, query, error).await;
            }
            Err(error) => return Err(error.into()),
        };
        if common::FailureDisposition::try_from(response.disposition)
            .ok()
            .is_none_or(|value| value == common::FailureDisposition::Unspecified)
        {
            return Err(WorkerError::Invalid("unknown failure disposition"));
        }
        super::blocking(move || journal::persist(&ack, &response)).await?;
    }
    Ok(())
}
