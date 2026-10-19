//! Authenticated worker RPCs, durable pull grants, and immutable reporting.
//!
//! The scheduling module adds durable admission and queue reconstruction (BS-014)
//! around this service. Native subprocess supervision belongs to BS-015.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use bergsmith_common::ids::{AttemptId, JobId, OrganizationId, TaskId, WorkerSessionId};
use bergsmith_common::job::JobState;
use bergsmith_common::task::AttemptState;
use bergsmith_persistence::{PersistenceError, attempts, jobs, plans, workers as ledger};
use bergsmith_protocol::cluster::{self as wire, cluster_service_server::ClusterService};
use bergsmith_protocol::convert::{
    attempt_state_to_wire, failure_disposition_to_wire, job_state_to_wire,
};
use bergsmith_protocol::hashing::{Sha256Digest, artifact_digest, receipt_digest};
use bergsmith_protocol::identity::ServiceToken;
use bergsmith_storage::{Location, ObjectStore};
use chrono::{DateTime, Utc};
use iceberg::spec::TableMetadata;
use prost::Message;
use sqlx::PgPool;
use tokio::sync::Notify;
use tonic::transport::server::{TcpConnectInfo, TlsConnectInfo};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::{planning, results};

/// Builds server-authenticated TLS for the worker endpoint.
/// Initializes rustls's process provider once because upstream HTTP clients enable
/// both providers. An already installed operator-selected provider is preserved.
#[must_use]
pub fn worker_tls_config(
    identity: tonic::transport::Identity,
) -> tonic::transport::ServerTlsConfig {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tonic::transport::ServerTlsConfig::new().identity(identity)
}

/// Token verifier configured by the operator; never contains a raw token.
#[derive(Clone, Debug)]
pub struct WorkerVerifier {
    /// Pool-scoped service identity.
    pub identity: ledger::Identity,
    /// Domain-separated verifier from `ServiceToken::verifier`.
    pub verifier: Sha256Digest,
}

/// Worker RPC timing and queue bounds.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// Advertised heartbeat interval.
    pub heartbeat_interval: Duration,
    /// Session and attempt lease duration.
    pub lease: Duration,
    /// Maximum time to await eligible work.
    pub long_poll_timeout: Duration,
    /// Maximum offered tasks retained in memory.
    pub queue_capacity: usize,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(15),
            lease: Duration::from_secs(90),
            long_poll_timeout: Duration::from_secs(30),
            queue_capacity: 256,
        }
    }
}

/// Catalog-vended access for a task. Values remain memory-only.
pub struct TaskAccess {
    /// Object store scoped to the table.
    pub store: ObjectStore,
    /// Table metadata used for footer verification.
    pub metadata: TableMetadata,
    /// Fresh table-scoped credentials; absent for local development storage.
    pub credentials: Option<wire::StorageCredentials>,
}

/// Supplies current table access from the coordinator's catalog configuration.
/// Implementations must not open database transactions across network I/O.
#[async_trait]
pub trait WorkerAccess: Send + Sync + 'static {
    /// Loads metadata, storage, and current catalog-vended credentials.
    ///
    /// # Errors
    /// Returns a redacted status for unavailable catalog or storage access.
    async fn access(&self, job: JobId) -> Result<TaskAccess, Status>;
}

/// An explicitly admitted task, ready for compatible workers in one pool.
#[derive(Clone, Debug)]
pub struct TaskOffer {
    /// Tenant owning the task.
    pub organization: OrganizationId,
    /// Durable task identifier.
    pub task: TaskId,
    /// Authorized pool.
    pub pool: String,
    /// Required worker capability.
    pub capability: String,
    /// Fetchable immutable task artifact, created by the admission caller.
    pub artifact: wire::ArtifactRef,
    /// Immutable execution envelope shared by every generation of this task.
    pub limits: wire::ResourceLimits,
}

/// Cloneable tonic service. All authoritative assignments live in PostgreSQL.
#[derive(Clone)]
pub struct WorkerService {
    pool: PgPool,
    epoch: i64,
    verifiers: Arc<Vec<WorkerVerifier>>,
    config: WorkerConfig,
    access: Arc<dyn WorkerAccess>,
    queue: Arc<Mutex<VecDeque<ReadyOffer>>>,
    changed: Arc<Notify>,
    polls: PollRegistry,
    admission: Arc<OnceLock<bergsmith_persistence::scheduling::AdmissionLimits>>,
    dispatch_ready: Arc<AtomicBool>,
}

type PollKey = (WorkerSessionId, Uuid);
type PollRegistry = Arc<Mutex<BTreeMap<PollKey, Weak<tokio::sync::Mutex<()>>>>>;

// Coalesces duplicate RPCs without retaining completed poll IDs in memory.
struct PollGate {
    key: PollKey,
    lock: Arc<tokio::sync::Mutex<()>>,
    registry: PollRegistry,
}

impl Drop for PollGate {
    fn drop(&mut self) {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if Arc::strong_count(&self.lock) == 1 {
            registry.remove(&self.key);
        }
    }
}

struct ReadyOffer {
    offer: TaskOffer,
    reserved_by: Option<WorkerSessionId>,
}

/// Keeps a candidate in the bounded queue during I/O, restoring it on cancellation.
struct Reservation {
    queue: Arc<Mutex<VecDeque<ReadyOffer>>>,
    changed: Arc<Notify>,
    offer: TaskOffer,
    consumed: bool,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = queue
            .iter()
            .position(|entry| entry.offer.task == self.offer.task)
        {
            if self.consumed {
                queue.remove(index);
            } else {
                queue[index].reserved_by = None;
            }
        }
        drop(queue);
        self.changed.notify_waiters();
    }
}

fn database(error: impl Into<PersistenceError>) -> Status {
    use bergsmith_protocol::common::{Failure, FailureDisposition};
    let (code, reason, message, disposition) = match error.into() {
        PersistenceError::LeadershipLost { .. } => (
            tonic::Code::Unavailable,
            "CoordinatorLeadershipLost",
            "coordinator leadership lost",
            FailureDisposition::RetrySameRequest,
        ),
        PersistenceError::Database(_) => (
            tonic::Code::Unavailable,
            "ExecutionLedgerUnavailable",
            "execution ledger unavailable",
            FailureDisposition::RetrySameRequest,
        ),
        PersistenceError::AdmissionUnavailable => (
            tonic::Code::Unavailable,
            "AdmissionCapacityUnavailable",
            "scheduler admission capacity unavailable",
            FailureDisposition::RetrySameRequest,
        ),
        PersistenceError::NotFound { .. } => (
            tonic::Code::NotFound,
            "WorkerExecutionEntityMissing",
            "worker execution entity not found",
            FailureDisposition::PermanentFailure,
        ),
        _ => (
            tonic::Code::FailedPrecondition,
            "WorkerAuthorityRejected",
            "worker execution authority rejected",
            FailureDisposition::PermanentFailure,
        ),
    };
    let failure = Failure {
        reason_code: reason.into(),
        disposition: disposition.into(),
        message: message.into(),
    };
    Status::with_details(code, message, failure.encode_to_vec().into())
}

fn id<T: std::str::FromStr>(value: &str) -> Result<T, Status> {
    value
        .parse()
        .map_err(|_| Status::invalid_argument("invalid identifier"))
}

fn scheduler_not_ready() -> Status {
    let failure = bergsmith_protocol::common::Failure {
        reason_code: "SchedulerNotReady".into(),
        disposition: bergsmith_protocol::common::FailureDisposition::RetrySameRequest.into(),
        message: "scheduler reconciliation is not ready".into(),
    };
    Status::with_details(
        tonic::Code::Unavailable,
        "scheduler reconciliation is not ready",
        failure.encode_to_vec().into(),
    )
}

fn timestamp(value: DateTime<Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: value.timestamp(),
        nanos: value.timestamp_subsec_nanos().cast_signed(),
    }
}

fn duration(value: Duration) -> prost_types::Duration {
    prost_types::Duration {
        seconds: i64::try_from(value.as_secs()).unwrap_or(i64::MAX),
        nanos: value.subsec_nanos().cast_signed(),
    }
}

impl WorkerService {
    /// Constructs the RPC service with an explicit leader epoch and access provider.
    ///
    /// # Errors
    /// Rejects zero or inconsistent timing/queue limits and duplicate verifiers.
    pub fn new(
        pool: PgPool,
        epoch: i64,
        verifiers: Vec<WorkerVerifier>,
        config: WorkerConfig,
        access: Arc<dyn WorkerAccess>,
    ) -> Result<Self, Status> {
        if epoch <= 0
            || config.heartbeat_interval.is_zero()
            || config.lease <= config.heartbeat_interval
            || config.long_poll_timeout.is_zero()
            || config.long_poll_timeout >= config.lease
            || config.queue_capacity == 0
            || verifiers
                .iter()
                .enumerate()
                .any(|(i, v)| verifiers[..i].iter().any(|p| p.verifier == v.verifier))
        {
            return Err(Status::invalid_argument(
                "invalid worker service configuration",
            ));
        }
        Ok(Self {
            pool,
            epoch,
            verifiers: Arc::new(verifiers),
            config,
            access,
            queue: Arc::new(Mutex::new(VecDeque::new())),
            changed: Arc::new(Notify::new()),
            polls: Arc::new(Mutex::new(BTreeMap::new())),
            admission: Arc::new(OnceLock::new()),
            dispatch_ready: Arc::new(AtomicBool::new(true)),
        })
    }

    pub(crate) fn managed(
        self,
        limits: bergsmith_persistence::scheduling::AdmissionLimits,
    ) -> Result<Self, Status> {
        self.admission
            .set(limits)
            .map_err(|_| Status::failed_precondition("worker service already has a scheduler"))?;
        self.set_dispatch_ready(false);
        Ok(self)
    }

    pub(crate) fn scheduler_context(&self) -> (PgPool, i64) {
        (self.pool.clone(), self.epoch)
    }

    pub(crate) fn set_dispatch_ready(&self, ready: bool) {
        self.dispatch_ready.store(ready, Ordering::Release);
        self.changed.notify_waiters();
    }

    pub(crate) fn reset_ready_queue(&self) -> usize {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.retain(|entry| entry.reserved_by.is_some());
        self.config.queue_capacity.saturating_sub(queue.len())
    }

    /// Returns cached task identifiers for diagnostics; membership is never authority.
    #[must_use]
    pub fn ready_tasks(&self) -> Vec<TaskId> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|entry| entry.offer.task)
            .collect()
    }

    fn poll_gate(&self, key: PollKey) -> PollGate {
        let mut registry = self
            .polls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lock = registry
            .get(&key)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                registry.insert(key, Arc::downgrade(&lock));
                lock
            });
        PollGate {
            key,
            lock,
            registry: Arc::clone(&self.polls),
        }
    }

    fn authenticate<T>(&self, request: &Request<T>) -> Result<ledger::Identity, Status> {
        if request
            .extensions()
            .get::<TlsConnectInfo<TcpConnectInfo>>()
            .is_none()
        {
            return Err(Status::unauthenticated("worker RPC requires TLS"));
        }
        let token = ServiceToken::from_metadata(request.metadata())
            .map_err(|_| Status::unauthenticated("invalid worker service token"))?;
        self.verifiers
            .iter()
            .find(|v| token.matches(&v.verifier) && token.pool() == v.identity.pool)
            .map(|v| v.identity.clone())
            .ok_or_else(|| Status::unauthenticated("invalid worker service token"))
    }

    async fn access(&self, job: JobId) -> Result<TaskAccess, Status> {
        tokio::time::timeout(Duration::from_secs(30), self.access.access(job))
            .await
            .map_err(|_| Status::unavailable("worker storage access timed out"))?
    }

    /// Offers an admitted task without polling PostgreSQL for queue selection.
    /// Validates the artifact against the immutable plan before enqueueing.
    ///
    /// # Errors
    /// Rejects invalid envelopes, artifacts, unknown tasks, and a full ready queue.
    pub async fn offer(&self, offer: TaskOffer) -> Result<(), Status> {
        self.validate_offer(&offer).await?;
        self.enqueue_registered(offer).await
    }

    // Registered refs were validated atomically once. Refill never reloads a
    // whole plan or fetches catalog credentials for each cached task.
    pub(crate) async fn enqueue_registered(&self, offer: TaskOffer) -> Result<(), Status> {
        validate_offer_shape(&offer)?;
        let mut conn = self.pool.acquire().await.map_err(database)?;
        if self.admission.get().is_some() {
            let stored = bergsmith_persistence::scheduling::dispatch_spec(&mut conn, offer.task)
                .await
                .map_err(database)?;
            if stored.as_ref() != Some(&crate::scheduling::dispatch_spec(&offer)?) {
                return Err(Status::failed_precondition(
                    "offer differs from durable dispatch configuration",
                ));
            }
        } else {
            ledger::bind_envelope(&mut conn, offer.task, &offer.limits.encode_to_vec())
                .await
                .map_err(database)?;
        }
        drop(conn);
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue
            .iter()
            .any(|existing| existing.offer.task == offer.task)
        {
            return Ok(());
        }
        if queue.len() >= self.config.queue_capacity {
            return Err(Status::resource_exhausted("worker ready queue full"));
        }
        queue.push_back(ReadyOffer {
            offer,
            reserved_by: None,
        });
        drop(queue);
        self.changed.notify_waiters();
        Ok(())
    }

    pub(crate) async fn validate_offer(&self, offer: &TaskOffer) -> Result<JobId, Status> {
        let mut conn = self.pool.acquire().await.map_err(database)?;
        let task = plans::task(&mut conn, offer.task).await.map_err(database)?;
        drop(conn);
        self.validate_offers(task.job_id, std::slice::from_ref(offer))
            .await?;
        Ok(task.job_id)
    }

    pub(crate) async fn validate_offers(
        &self,
        job: JobId,
        offers: &[TaskOffer],
    ) -> Result<(), Status> {
        let first = offers
            .first()
            .ok_or_else(|| Status::invalid_argument("missing task offers"))?;
        let mut conn = self.pool.acquire().await.map_err(database)?;
        jobs::job(&mut conn, first.organization, job)
            .await
            .map_err(database)?;
        let plan = planning::load_plan(&mut conn, job)
            .await
            .map_err(|_| Status::failed_precondition("invalid stored plan"))?
            .ok_or_else(|| Status::failed_precondition("missing stored plan"))?;
        drop(conn);
        let tasks: BTreeMap<_, _> = plan
            .tasks()
            .iter()
            .map(|task| (task.task_id(), task))
            .collect();
        let mut expected = Vec::with_capacity(offers.len());
        for offer in offers {
            validate_offer_shape(offer)?;
            if offer.organization != first.organization {
                return Err(Status::invalid_argument("task offer organization mismatch"));
            }
            let task = tasks
                .get(&offer.task)
                .ok_or_else(|| Status::failed_precondition("task absent from plan"))?;
            let artifact = plan
                .artifact_of(task)
                .map_err(|_| Status::failed_precondition("invalid task artifact"))?;
            let digest = artifact_digest(&artifact);
            if offer.artifact.sha256 != digest.as_bytes() {
                return Err(Status::invalid_argument("task artifact digest mismatch"));
            }
            expected.push((offer, digest));
        }
        let access = self.access(job).await?;
        for (offer, digest) in expected {
            let location = Location::parse(&offer.artifact.uri)
                .map_err(|_| Status::invalid_argument("invalid artifact URI"))?;
            let bytes = tokio::time::timeout(Duration::from_secs(30), access.store.read(&location))
                .await
                .map_err(|_| Status::unavailable("task artifact read timed out"))?
                .map_err(|_| Status::unavailable("task artifact unavailable"))?;
            let actual = bergsmith_protocol::artifact::TaskArtifact::decode(bytes)
                .map_err(|_| Status::invalid_argument("invalid task artifact"))?;
            if artifact_digest(&actual) != digest {
                return Err(Status::invalid_argument("task artifact content mismatch"));
            }
        }
        Ok(())
    }

    async fn status(
        &self,
        identity: &ledger::Identity,
        session: WorkerSessionId,
        attempt_id: AttemptId,
    ) -> Result<wire::GetAttemptStatusResponse, Status> {
        let mut conn = self.pool.acquire().await.map_err(database)?;
        let attempt = ledger::owned_attempt(&mut conn, identity, session, attempt_id)
            .await
            .map_err(database)?;
        let task = plans::task(&mut conn, attempt.task_id)
            .await
            .map_err(database)?;
        let job = jobs::job(&mut conn, identity.organization, task.job_id)
            .await
            .map_err(database)?;
        let live: bool = sqlx::query_scalar("SELECT $1::timestamptz > clock_timestamp()")
            .bind(attempt.lease_expires_at)
            .fetch_one(&mut *conn)
            .await
            .map_err(database)?;
        let session_live = ledger::attempt_session_is_live(&mut conn, session)
            .await
            .map_err(database)?;
        let outcome = if attempt.state == AttemptState::Accepted {
            wire::ResultOutcome::AlreadyAccepted
        } else if matches!(job.state, JobState::Cancelling | JobState::Cancelled) {
            wire::ResultOutcome::Cancelled
        } else if attempt.state.is_terminal()
            || !live
            || !session_live
            || job.state != JobState::Running
        {
            wire::ResultOutcome::StaleAttempt
        } else {
            wire::ResultOutcome::Unspecified
        };
        Ok(wire::GetAttemptStatusResponse {
            attempt_state: attempt_state_to_wire(attempt.state),
            result_outcome: outcome.into(),
            job_state: job_state_to_wire(job.state),
        })
    }

    async fn replay(
        &self,
        identity: &ledger::Identity,
        session: WorkerSessionId,
        poll: ledger::Poll,
    ) -> Result<wire::RequestTaskResponse, Status> {
        let Some(attempt_id) = poll.attempt_id else {
            return Ok(no_task());
        };
        let attempt_id = AttemptId::from_uuid(attempt_id);
        let status = self.status(identity, session, attempt_id).await?;
        if status.result_outcome != i32::from(wire::ResultOutcome::Unspecified) {
            return Ok(wire::RequestTaskResponse {
                result: Some(wire::request_task_response::Result::Disposition(status)),
            });
        }
        let mut grant = wire::TaskGrant::decode(
            poll.grant_bytes
                .as_deref()
                .ok_or_else(|| Status::internal("missing stored grant"))?,
        )
        .map_err(|_| Status::internal("invalid stored grant"))?;
        let job = id(&grant.job_id)?;
        let access = self.access(job).await?;
        let status = self.status(identity, session, attempt_id).await?;
        if status.result_outcome != i32::from(wire::ResultOutcome::Unspecified) {
            return Ok(wire::RequestTaskResponse {
                result: Some(wire::request_task_response::Result::Disposition(status)),
            });
        }
        let mut conn = self.pool.acquire().await.map_err(database)?;
        let attempt = ledger::owned_attempt(&mut conn, identity, session, attempt_id)
            .await
            .map_err(database)?;
        grant.lease_expires_at = Some(timestamp(attempt.lease_expires_at));
        grant.credentials = access.credentials;
        Ok(wire::RequestTaskResponse {
            result: Some(wire::request_task_response::Result::Grant(grant)),
        })
    }

    fn reserve(
        &self,
        identity: &ledger::Identity,
        session_id: WorkerSessionId,
        session: &ledger::Session,
    ) -> Option<Reservation> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if queue
            .iter()
            .any(|entry| entry.reserved_by == Some(session_id))
        {
            return None;
        }
        let entry = queue.iter_mut().find(|entry| {
            let offer = &entry.offer;
            entry.reserved_by.is_none()
                && offer.organization == identity.organization
                && offer.pool == identity.pool
                && session.capabilities.contains(&offer.capability)
                && offer.limits.memory_bytes <= u64::try_from(session.memory_bytes).unwrap_or(0)
                && offer.limits.cpu_millis <= u32::try_from(session.cpu_millis).unwrap_or(0)
        })?;
        entry.reserved_by = Some(session_id);
        Some(Reservation {
            queue: self.queue.clone(),
            changed: self.changed.clone(),
            offer: entry.offer.clone(),
            consumed: false,
        })
    }

    async fn grant_reserved(
        &self,
        identity: &ledger::Identity,
        session: WorkerSessionId,
        request: Uuid,
        reservation: &mut Reservation,
    ) -> Result<Option<ledger::Poll>, Status> {
        let offer = &reservation.offer;
        let mut conn = self.pool.acquire().await.map_err(database)?;
        let task = plans::task(&mut conn, offer.task).await.map_err(database)?;
        let job = jobs::job(&mut conn, identity.organization, task.job_id)
            .await
            .map_err(database)?;
        if task.state != bergsmith_common::task::TaskState::Pending
            || !matches!(job.state, JobState::Queued | JobState::Running)
            || job.cancellation.is_some()
        {
            reservation.consumed = true;
            return Ok(None);
        }
        let plan = planning::load_plan(&mut conn, task.job_id)
            .await
            .map_err(|_| Status::failed_precondition("invalid stored plan"))?
            .ok_or_else(|| Status::failed_precondition("missing stored plan"))?;
        let (attempt_id, grant) = draft_grant(offer, &task, &plan, session)?;
        let new = attempts::NewGrant {
            attempt_id,
            task_id: task.id,
            worker_session_id: session,
            lease: self.config.lease,
        };
        let payload = ledger::GrantPayload {
            grant_bytes: &grant.encode_to_vec(),
            output_prefix: &grant.output_prefix,
            envelope_bytes: &offer.limits.encode_to_vec(),
        };
        let poll = if let Some(limits) = self.admission.get() {
            if !self.dispatch_ready.load(Ordering::Acquire) {
                return Err(scheduler_not_ready());
            }
            ledger::grant_admitted(
                &mut conn, identity, &new, request, self.epoch, &payload, *limits,
            )
            .await
        } else {
            ledger::grant(&mut conn, identity, &new, request, self.epoch, &payload).await
        }
        .map_err(database)?;
        reservation.consumed = poll.attempt_id == Some(*attempt_id.as_uuid());
        Ok(Some(poll))
    }

    async fn pull(
        &self,
        identity: &ledger::Identity,
        session_id: WorkerSessionId,
        request: Uuid,
    ) -> Result<wire::RequestTaskResponse, Status> {
        let deadline = tokio::time::Instant::now() + self.config.long_poll_timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let mut conn = self.pool.acquire().await.map_err(database)?;
            let session = ledger::session(&mut conn, identity, session_id)
                .await
                .map_err(database)?;
            if let Some(poll) = ledger::poll(&mut conn, session_id, request)
                .await
                .map_err(database)?
            {
                drop(conn);
                return self.replay(identity, session_id, poll).await;
            }
            if !self.dispatch_ready.load(Ordering::Acquire) {
                return Err(scheduler_not_ready());
            }
            let busy: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM attempts
                WHERE worker_session_id = $1 AND state IN ('Granted', 'Running'))",
            )
            .bind(session_id.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .map_err(database)?;
            drop(conn);
            if !session.draining
                && !busy
                && let Some(mut reservation) = self.reserve(identity, session_id, &session)
            {
                let poll = tokio::time::timeout(
                    Duration::from_secs(30),
                    self.grant_reserved(identity, session_id, request, &mut reservation),
                )
                .await
                .map_err(|_| Status::deadline_exceeded("task grant decision timed out"))??;
                drop(reservation);
                if let Some(poll) = poll {
                    return self.replay(identity, session_id, poll).await;
                }
                continue;
            }
            if session.draining || tokio::time::timeout_at(deadline, notified).await.is_err() {
                let mut conn = self.pool.acquire().await.map_err(database)?;
                let poll = ledger::finish_empty_poll(&mut conn, identity, session_id, request)
                    .await
                    .map_err(database)?;
                drop(conn);
                return self.replay(identity, session_id, poll).await;
            }
        }
    }
}

fn no_task() -> wire::RequestTaskResponse {
    wire::RequestTaskResponse {
        result: Some(wire::request_task_response::Result::NoTask(wire::NoTask {
            retry_after: Some(duration(Duration::from_secs(1))),
        })),
    }
}

fn draft_grant(
    offer: &TaskOffer,
    task: &plans::Task,
    plan: &bergsmith_maintenance::PlanDocument,
    session: WorkerSessionId,
) -> Result<(AttemptId, wire::TaskGrant), Status> {
    let attempt_id = AttemptId::generate();
    let prefix = Location::parse(plan.baseline().data_location())
        .map_err(|_| Status::internal("invalid planned data location"))?
        .attempt_output_prefix(task.job_id, attempt_id);
    Ok((
        attempt_id,
        wire::TaskGrant {
            job_id: task.job_id.to_string(),
            plan_id: task.plan_id.to_string(),
            task_id: task.id.to_string(),
            attempt_id: attempt_id.to_string(),
            worker_session_id: session.to_string(),
            generation: task.attempts_started + 1,
            lease_expires_at: None,
            artifact: Some(offer.artifact.clone()),
            input_state_id: plan
                .sha256()
                .map_err(|_| Status::internal("invalid stored plan"))?
                .to_hex(),
            capability_profile: offer.capability.clone(),
            output_prefix: prefix.to_string(),
            limits: Some(offer.limits),
            credentials: None,
            metadata_location: plan.baseline().metadata_location().to_owned(),
            max_manifest_entries: plan.config().max_manifest_entries,
        },
    ))
}

#[tonic::async_trait]
impl ClusterService for WorkerService {
    async fn register_worker(
        &self,
        request: Request<wire::RegisterWorkerRequest>,
    ) -> Result<Response<wire::RegisterWorkerResponse>, Status> {
        let identity = self.authenticate(&request)?;
        let registration = request.into_inner();
        if registration.pool != identity.pool {
            return Err(Status::permission_denied("worker pool mismatch"));
        }
        if registration.protocol_revision < 2 {
            return Err(Status::failed_precondition("unsupported worker protocol"));
        }
        let capacity = registration
            .capacity
            .ok_or_else(|| Status::invalid_argument("missing worker capacity"))?;
        if capacity.max_concurrent_tasks != 1 {
            return Err(Status::invalid_argument(
                "one executing task per worker is required",
            ));
        }
        let session = WorkerSessionId::generate();
        let mut conn = self.pool.acquire().await.map_err(database)?;
        ledger::register(
            &mut conn,
            &identity,
            &ledger::Registration {
                id: session,
                worker_name: registration.worker_name,
                capabilities: registration.capabilities,
                memory_bytes: i64::try_from(capacity.memory_bytes)
                    .map_err(|_| Status::invalid_argument("invalid worker memory"))?,
                cpu_millis: i32::try_from(capacity.cpu_millis)
                    .map_err(|_| Status::invalid_argument("invalid worker CPU"))?,
                build_version: registration.build_version,
            },
            self.config.lease,
        )
        .await
        .map_err(database)?;
        Ok(Response::new(wire::RegisterWorkerResponse {
            worker_session_id: session.to_string(),
            protocol_revision: 2,
            heartbeat_interval: Some(duration(self.config.heartbeat_interval)),
            long_poll_timeout: Some(duration(self.config.long_poll_timeout)),
        }))
    }

    async fn request_task(
        &self,
        request: Request<wire::RequestTaskRequest>,
    ) -> Result<Response<wire::RequestTaskResponse>, Status> {
        let identity = self.authenticate(&request)?;
        let poll = request.into_inner();
        let session = id(&poll.worker_session_id)?;
        let request_id = id(&poll.request_id)?;
        // A duplicate must await the original durable decision, not race it by
        // persisting an empty result when its own long-poll deadline expires.
        let gate = self.poll_gate((session, request_id));
        let _guard = gate.lock.lock().await;
        Ok(Response::new(
            self.pull(&identity, session, request_id).await?,
        ))
    }

    async fn heartbeat(
        &self,
        request: Request<wire::HeartbeatRequest>,
    ) -> Result<Response<wire::HeartbeatResponse>, Status> {
        let identity = self.authenticate(&request)?;
        let heartbeat = request.into_inner();
        let session_id = id(&heartbeat.worker_session_id)?;
        if heartbeat.attempts.len() > 1 {
            return Err(Status::invalid_argument("too many executing attempts"));
        }
        let mut conn = self.pool.acquire().await.map_err(database)?;
        let session = ledger::touch(
            &mut conn,
            &identity,
            session_id,
            self.epoch,
            self.config.lease,
        )
        .await
        .map_err(database)?;
        drop(conn);
        let mut directives = Vec::new();
        for lease in heartbeat.attempts {
            let attempt_id = id(&lease.attempt_id)?;
            let mut conn = self.pool.acquire().await.map_err(database)?;
            let renewal = ledger::renew(
                &mut conn,
                &identity,
                session_id,
                attempt_id,
                lease.generation,
                self.epoch,
                self.config.lease,
            )
            .await
            .map_err(database)?;
            let directive = match renewal {
                ledger::Renewal::Renewed(deadline) => {
                    wire::attempt_directive::Directive::LeaseExpiresAt(timestamp(deadline))
                }
                ledger::Renewal::Cancel => {
                    wire::attempt_directive::Directive::Cancel(wire::Cancel {
                        reason: "job cancellation requested".into(),
                        grace_period: Some(duration(Duration::from_secs(5))),
                    })
                }
                ledger::Renewal::Fenced => {
                    wire::attempt_directive::Directive::Fenced(wire::Fenced {})
                }
            };
            let refresh_job = if matches!(renewal, ledger::Renewal::Renewed(_)) {
                let attempt = attempts::attempt(&mut conn, attempt_id)
                    .await
                    .map_err(database)?;
                let task = plans::task(&mut conn, attempt.task_id)
                    .await
                    .map_err(database)?;
                Some(task.job_id)
            } else {
                None
            };
            drop(conn);
            let credentials = if let Some(job) = refresh_job {
                self.access(job).await?.credentials
            } else {
                None
            };
            directives.push(wire::AttemptDirective {
                attempt_id: lease.attempt_id,
                directive: Some(directive),
                refreshed_credentials: credentials,
            });
        }
        Ok(Response::new(wire::HeartbeatResponse {
            directives,
            drain: session.draining,
        }))
    }

    async fn report_task_result(
        &self,
        request: Request<wire::ReportTaskResultRequest>,
    ) -> Result<Response<wire::ReportTaskResultResponse>, Status> {
        let identity = self.authenticate(&request)?;
        let report = request.into_inner();
        let receipt = report
            .receipt
            .ok_or_else(|| Status::invalid_argument("missing receipt"))?;
        let session_id = id(&report.worker_session_id)?;
        let attempt_id = id(&receipt.attempt_id)?;
        let mut conn = self.pool.acquire().await.map_err(database)?;
        let attempt = ledger::owned_attempt(&mut conn, &identity, session_id, attempt_id)
            .await
            .map_err(database)?;
        let task = plans::task(&mut conn, attempt.task_id)
            .await
            .map_err(database)?;
        let job = jobs::job(&mut conn, identity.organization, task.job_id)
            .await
            .map_err(database)?;
        let invalid = || {
            Response::new(wire::ReportTaskResultResponse {
                outcome: wire::ResultOutcome::InvalidResult.into(),
                job_state: job_state_to_wire(job.state),
            })
        };
        if receipt.generation != attempt.generation {
            return Ok(Response::new(wire::ReportTaskResultResponse {
                outcome: wire::ResultOutcome::StaleAttempt.into(),
                job_state: job_state_to_wire(job.state),
            }));
        }
        if receipt.task_id != task.id.to_string()
            || report.receipt_sha256 != receipt_digest(&receipt).as_bytes()
        {
            return Ok(invalid());
        }
        // Check the submitted digest under the same locks used by acceptance.
        // An attempt-status snapshot alone cannot acknowledge a specific receipt.
        let mut canonical = receipt.clone();
        bergsmith_protocol::hashing::canonicalize_receipt(&mut canonical);
        let submission = attempts::ReceiptSubmission {
            attempt_id,
            generation: receipt.generation,
            content: serde_json::json!(canonical.encode_to_vec()),
            content_hash: receipt_digest(&canonical).to_hex(),
        };
        match attempts::receipt_status(&mut conn, &submission).await {
            Ok(Some(acceptance)) => return Ok(Response::new(result_response(acceptance))),
            Err(PersistenceError::ReceiptConflict { .. }) => return Ok(invalid()),
            Err(error) => return Err(database(error)),
            Ok(None) => {}
        }
        let plan = planning::load_plan(&mut conn, task.job_id)
            .await
            .map_err(|_| Status::failed_precondition("invalid stored plan"))?
            .ok_or_else(|| Status::failed_precondition("missing stored plan"))?;
        drop(conn);
        let access = self.access(task.job_id).await?;
        let verified =
            match results::verify_receipt(&plan, &canonical, &access.store, &access.metadata).await
            {
                Ok(verified) => verified,
                Err(error) if invalid_output(&error) => return Ok(invalid()),
                Err(_) => return Err(Status::unavailable("task result verification unavailable")),
            };
        let mut conn = self.pool.acquire().await.map_err(database)?;
        match bergsmith_persistence::publications::accept_outputs(
            &mut conn,
            &submission,
            &verified.prefix.to_string(),
            &verified.objects,
        )
        .await
        {
            Ok(acceptance) => Ok(Response::new(result_response(acceptance))),
            Err(PersistenceError::ReceiptConflict { .. }) => Ok(invalid()),
            Err(error) => Err(database(error)),
        }
    }

    async fn get_attempt_status(
        &self,
        request: Request<wire::GetAttemptStatusRequest>,
    ) -> Result<Response<wire::GetAttemptStatusResponse>, Status> {
        let identity = self.authenticate(&request)?;
        let query = request.into_inner();
        Ok(Response::new(
            self.status(
                &identity,
                id(&query.worker_session_id)?,
                id(&query.attempt_id)?,
            )
            .await?,
        ))
    }

    async fn drain_worker(
        &self,
        request: Request<wire::DrainWorkerRequest>,
    ) -> Result<Response<wire::DrainWorkerResponse>, Status> {
        let identity = self.authenticate(&request)?;
        let session = id(&request.into_inner().worker_session_id)?;
        let mut conn = self.pool.acquire().await.map_err(database)?;
        ledger::drain(&mut conn, &identity, session)
            .await
            .map_err(database)?;
        self.changed.notify_waiters();
        Ok(Response::new(wire::DrainWorkerResponse {}))
    }

    async fn report_task_failure(
        &self,
        request: Request<wire::ReportTaskFailureRequest>,
    ) -> Result<Response<wire::ReportTaskFailureResponse>, Status> {
        let identity = self.authenticate(&request)?;
        let report = request.into_inner();
        let session = id(&report.worker_session_id)?;
        let attempt_id = id(&report.attempt_id)?;
        let failure = report
            .failure
            .ok_or_else(|| Status::invalid_argument("missing failure"))?;
        let mut conn = self.pool.acquire().await.map_err(database)?;
        let attempt = ledger::owned_attempt(&mut conn, &identity, session, attempt_id)
            .await
            .map_err(database)?;
        if report.generation != attempt.generation {
            return Err(Status::failed_precondition("stale attempt generation"));
        }
        let response = ledger::report_failure(
            &mut conn,
            &identity,
            session,
            attempt_id,
            report.generation,
            self.epoch,
            &failure.reason_code,
        )
        .await
        .map_err(database)?;
        Ok(Response::new(wire::ReportTaskFailureResponse {
            disposition: failure_disposition_to_wire(response.0),
            job_state: job_state_to_wire(response.1),
        }))
    }
}

fn result_response(acceptance: attempts::Acceptance) -> wire::ReportTaskResultResponse {
    let outcome = match acceptance.outcome {
        attempts::AcceptOutcome::Accepted => wire::ResultOutcome::Accepted,
        attempts::AcceptOutcome::AlreadyAccepted => wire::ResultOutcome::AlreadyAccepted,
        attempts::AcceptOutcome::StaleAttempt => wire::ResultOutcome::StaleAttempt,
        attempts::AcceptOutcome::Cancelled => wire::ResultOutcome::Cancelled,
    };
    wire::ReportTaskResultResponse {
        outcome: outcome.into(),
        job_state: job_state_to_wire(acceptance.job_state),
    }
}

fn invalid_output(error: &results::ResultError) -> bool {
    match error {
        results::ResultError::Invalid(_) | results::ResultError::Location(_) => true,
        results::ResultError::Parquet(error) => {
            !matches!(error, parquet::errors::ParquetError::External(_))
        }
        results::ResultError::Iceberg(error) => matches!(
            error.kind(),
            iceberg::ErrorKind::DataInvalid | iceberg::ErrorKind::FeatureUnsupported
        ),
        _ => false,
    }
}

fn validate_offer_shape(offer: &TaskOffer) -> Result<(), Status> {
    if offer.pool.is_empty()
        || offer.capability.is_empty()
        || offer.capability.len() > 200
        || offer.limits.memory_bytes == 0
        || offer.limits.cpu_millis == 0
        || offer.limits.max_output_bytes == 0
        || !offer.limits.max_duration.as_ref().is_some_and(|d| {
            d.seconds >= 0
                && (0..1_000_000_000).contains(&d.nanos)
                && (d.seconds > 0 || d.nanos > 0)
        })
    {
        return Err(Status::invalid_argument("invalid task offer"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_output_is_distinct_from_external_verification_failures() {
        use parquet::errors::ParquetError;
        for error in [
            results::ResultError::Invalid("partition mismatch"),
            results::ResultError::Location(bergsmith_storage::LocationError::MissingScheme(
                "invalid".into(),
            )),
            results::ResultError::Parquet(ParquetError::General("invalid footer".into())),
            results::ResultError::Parquet(ParquetError::EOF("truncated footer".into())),
            results::ResultError::Iceberg(iceberg::Error::new(
                iceberg::ErrorKind::DataInvalid,
                "invalid metadata",
            )),
            results::ResultError::Iceberg(iceberg::Error::new(
                iceberg::ErrorKind::FeatureUnsupported,
                "unsupported encoding",
            )),
        ] {
            assert!(invalid_output(&error));
        }
        for error in [
            results::ResultError::Timeout,
            results::ResultError::Parquet(ParquetError::External(Box::new(std::io::Error::from(
                std::io::ErrorKind::ConnectionReset,
            )))),
            results::ResultError::Iceberg(iceberg::Error::new(
                iceberg::ErrorKind::Unexpected,
                "temporary failure",
            )),
            results::ResultError::Plan(bergsmith_maintenance::PlanError::CorruptPlan(
                "stored plan".into(),
            )),
        ] {
            assert!(!invalid_output(&error));
        }
    }

    struct NoAccess;

    #[async_trait]
    impl WorkerAccess for NoAccess {
        async fn access(&self, _: JobId) -> Result<TaskAccess, Status> {
            Err(Status::unavailable("no table access in queue test"))
        }
    }

    #[tokio::test]
    async fn cancelling_a_reservation_restores_its_task_without_losing_capacity() {
        let identity = ledger::Identity {
            organization: OrganizationId::generate(),
            principal: bergsmith_common::ids::PrincipalId::generate(),
            pool: "native".into(),
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/queue_test")
            .unwrap();
        let service = WorkerService::new(
            pool,
            1,
            vec![],
            WorkerConfig {
                queue_capacity: 1,
                ..WorkerConfig::default()
            },
            Arc::new(NoAccess),
        )
        .unwrap();
        let poll_key = (WorkerSessionId::generate(), Uuid::now_v7());
        let gate = service.poll_gate(poll_key);
        let duplicate = service.poll_gate(poll_key);
        let independent = service.poll_gate((poll_key.0, Uuid::now_v7()));
        let guard = gate.lock.lock().await;
        assert!(duplicate.lock.try_lock().is_err());
        assert!(independent.lock.try_lock().is_ok());
        drop(guard);
        drop(gate);
        assert_eq!(service.polls.lock().unwrap().len(), 2);
        drop(duplicate);
        drop(independent);
        assert!(service.polls.lock().unwrap().is_empty());
        let offer = TaskOffer {
            organization: identity.organization,
            task: TaskId::generate(),
            pool: identity.pool.clone(),
            capability: "native-v1".into(),
            artifact: wire::ArtifactRef {
                uri: "memory://queue/task.pb".into(),
                sha256: vec![0; 32],
            },
            limits: wire::ResourceLimits {
                memory_bytes: 1,
                cpu_millis: 1,
                max_duration: Some(duration(Duration::from_secs(1))),
                max_output_bytes: 1,
            },
        };
        service.queue.lock().unwrap().push_back(ReadyOffer {
            offer,
            reserved_by: None,
        });
        let session = ledger::Session {
            capabilities: vec!["native-v1".into()],
            memory_bytes: 1,
            cpu_millis: 1,
            draining: false,
            lease_expires_at: DateTime::UNIX_EPOCH,
        };
        let worker = WorkerSessionId::generate();
        let reserved = service.reserve(&identity, worker, &session).unwrap();
        assert!(
            service
                .reserve(&identity, WorkerSessionId::generate(), &session)
                .is_none()
        );
        assert!(service.reserve(&identity, worker, &session).is_none());
        drop(reserved);
        let mut restored = service.reserve(&identity, worker, &session).unwrap();
        restored.consumed = true;
        drop(restored);
        assert!(service.reserve(&identity, worker, &session).is_none());
    }
}
