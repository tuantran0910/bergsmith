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
use bergsmith_persistence::{
    PersistenceError, attempts, jobs, plans, worker_tokens, workers as ledger,
};
use bergsmith_protocol::cluster::{self as wire, cluster_service_server::ClusterService};
use bergsmith_protocol::common::FailureDisposition as Disposition;
use bergsmith_protocol::convert::{
    attempt_state_to_wire, failure_disposition_to_wire, job_state_to_wire,
};
use bergsmith_protocol::hashing::{artifact_digest, receipt_digest};
use bergsmith_protocol::identity::ServiceToken;
use bergsmith_storage::{Location, ObjectStore};
use chrono::{DateTime, Utc};
use iceberg::spec::TableMetadata;
use prost::Message;
use sqlx::PgPool;
use tokio::sync::Notify;
use tokio::time::Instant;
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
    config: WorkerConfig,
    access: Arc<dyn WorkerAccess>,
    queue: Arc<Mutex<VecDeque<ReadyOffer>>>,
    changed: Arc<Notify>,
    polls: PollRegistry,
    admission: Arc<OnceLock<bergsmith_persistence::scheduling::AdmissionLimits>>,
    dispatch_ready: Arc<AtomicBool>,
    delivered: CredentialLedger,
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

/// Builds a status carrying the typed `Failure` detail required at the worker
/// boundary: a stable reason code and its retry disposition.
fn rejection(
    code: tonic::Code,
    reason: &'static str,
    message: &'static str,
    disposition: Disposition,
) -> Status {
    let failure = bergsmith_protocol::common::Failure {
        reason_code: reason.into(),
        disposition: disposition.into(),
        message: message.into(),
    };
    Status::with_details(code, message, failure.encode_to_vec().into())
}

/// A malformed request that repeating cannot fix.
fn invalid_request(reason: &'static str, message: &'static str) -> Status {
    rejection(
        tonic::Code::InvalidArgument,
        reason,
        message,
        Disposition::PermanentFailure,
    )
}

/// The caller's service identity is missing or revoked.
fn unauthenticated(reason: &'static str, message: &'static str) -> Status {
    rejection(
        tonic::Code::Unauthenticated,
        reason,
        message,
        Disposition::PermanentFailure,
    )
}

/// Durable or configured state forbids the request permanently.
fn precondition(reason: &'static str, message: &'static str) -> Status {
    rejection(
        tonic::Code::FailedPrecondition,
        reason,
        message,
        Disposition::PermanentFailure,
    )
}

/// A transient condition; the caller repeats the same request or receipt.
fn unavailable(reason: &'static str, message: &'static str) -> Status {
    rejection(
        tonic::Code::Unavailable,
        reason,
        message,
        Disposition::RetrySameRequest,
    )
}

/// Returns the typed reason carried by a status, empty when it has none.
fn reason_code(status: &Status) -> String {
    bergsmith_protocol::common::Failure::decode(status.details())
        .map(|failure| failure.reason_code)
        .unwrap_or_default()
}

/// Gives an access-provider status a typed reason when it lacks one. Transient
/// catalog or storage outages retry the same request; other refusals are
/// permanent. The provider's redacted message is preserved.
fn typed_access_error(status: Status) -> Status {
    if !reason_code(&status).is_empty() {
        return status;
    }
    let (reason, disposition) = match status.code() {
        tonic::Code::Unavailable
        | tonic::Code::DeadlineExceeded
        | tonic::Code::ResourceExhausted
        | tonic::Code::Aborted => (
            "WorkerTableAccessUnavailable",
            Disposition::RetrySameRequest,
        ),
        _ => ("WorkerTableAccessRejected", Disposition::PermanentFailure),
    };
    let failure = bergsmith_protocol::common::Failure {
        reason_code: reason.into(),
        disposition: disposition.into(),
        message: status.message().into(),
    };
    Status::with_details(
        status.code(),
        status.message(),
        failure.encode_to_vec().into(),
    )
}

fn database(error: impl Into<PersistenceError>) -> Status {
    match error.into() {
        PersistenceError::LeadershipLost { .. } => {
            unavailable("CoordinatorLeadershipLost", "coordinator leadership lost")
        }
        PersistenceError::Database(_) => {
            unavailable("ExecutionLedgerUnavailable", "execution ledger unavailable")
        }
        PersistenceError::AdmissionUnavailable => unavailable(
            "AdmissionCapacityUnavailable",
            "scheduler admission capacity unavailable",
        ),
        PersistenceError::NotFound { .. } => rejection(
            tonic::Code::NotFound,
            "WorkerExecutionEntityMissing",
            "worker execution entity not found",
            Disposition::PermanentFailure,
        ),
        _ => precondition(
            "WorkerAuthorityRejected",
            "worker execution authority rejected",
        ),
    }
}

fn id<T: std::str::FromStr>(value: &str) -> Result<T, Status> {
    value
        .parse()
        .map_err(|_| invalid_request("InvalidIdentifier", "invalid identifier"))
}

fn scheduler_not_ready() -> Status {
    unavailable("SchedulerNotReady", "scheduler reconciliation is not ready")
}

fn invalid_stored_plan() -> Status {
    precondition("StoredPlanInvalid", "invalid stored plan")
}

fn missing_stored_plan() -> Status {
    precondition("StoredPlanMissing", "missing stored plan")
}

fn verification_unavailable() -> Status {
    unavailable(
        "TaskResultVerificationUnavailable",
        "task result verification unavailable",
    )
}

/// Whether a grant error proves this offer can never be granted, so keeping it
/// queued would only make every compatible poll hit it again. Session-scoped,
/// leadership, admission, and database errors leave the offer queued.
fn offer_unusable(error: &PersistenceError) -> bool {
    match error {
        PersistenceError::NotFound { entity, .. } => matches!(*entity, "task" | "job"),
        PersistenceError::InvalidValue { entity, .. } => *entity == "task resource envelope",
        PersistenceError::JobNotInState { .. }
        | PersistenceError::JobTransition(_)
        | PersistenceError::TaskTransition(_)
        | PersistenceError::RetryBudgetExhausted { .. }
        | PersistenceError::AlreadyExists { .. } => true,
        _ => false,
    }
}

/// Expiry of the storage credentials last delivered for an attempt. Memory-only;
/// it never holds credential values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Delivered {
    /// The access provider vended no credentials at this monotonic instant.
    /// Forget this hint after one lease even if the worker never reports back.
    NotVended(Instant),
    /// The delivered credentials expire at this catalog-reported time.
    Expires(DateTime<Utc>),
}

type CredentialLedger = Arc<Mutex<BTreeMap<AttemptId, Delivered>>>;

fn expiry(credentials: &wire::StorageCredentials) -> Option<DateTime<Utc>> {
    let at = credentials.expires_at.as_ref()?;
    DateTime::from_timestamp(at.seconds, u32::try_from(at.nanos).ok()?)
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
    /// Rejects zero or inconsistent timing/queue limits.
    pub fn new(
        pool: PgPool,
        epoch: i64,
        config: WorkerConfig,
        access: Arc<dyn WorkerAccess>,
    ) -> Result<Self, Status> {
        if epoch <= 0
            || config.heartbeat_interval.is_zero()
            || config.lease <= config.heartbeat_interval
            || config.long_poll_timeout.is_zero()
            || config.long_poll_timeout >= config.lease
            || config.queue_capacity == 0
        {
            return Err(invalid_request(
                "WorkerServiceConfigurationInvalid",
                "invalid worker service configuration",
            ));
        }
        Ok(Self {
            pool,
            epoch,
            config,
            access,
            queue: Arc::new(Mutex::new(VecDeque::new())),
            changed: Arc::new(Notify::new()),
            polls: Arc::new(Mutex::new(BTreeMap::new())),
            admission: Arc::new(OnceLock::new()),
            dispatch_ready: Arc::new(AtomicBool::new(true)),
            delivered: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub(crate) fn managed(
        self,
        limits: bergsmith_persistence::scheduling::AdmissionLimits,
    ) -> Result<Self, Status> {
        self.admission.set(limits).map_err(|_| {
            precondition(
                "WorkerSchedulerAlreadyAttached",
                "worker service already has a scheduler",
            )
        })?;
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

    async fn authenticate<T>(&self, request: &Request<T>) -> Result<ledger::Identity, Status> {
        if request
            .extensions()
            .get::<TlsConnectInfo<TcpConnectInfo>>()
            .is_none()
        {
            return Err(unauthenticated(
                "WorkerTlsRequired",
                "worker RPC requires TLS",
            ));
        }
        let token = ServiceToken::from_metadata(request.metadata())
            .map_err(|_| unauthenticated("WorkerTokenInvalid", "invalid worker service token"))?;
        // The verifier ledger is read on every call, so revocation and
        // principal deactivation apply without restarting the coordinator.
        let authenticated = tokio::time::timeout(Duration::from_secs(5), async {
            let mut conn = self.pool.acquire().await?;
            worker_tokens::authenticate(&mut conn, token.pool(), &token.verifier()).await
        })
        .await
        .map_err(|_| {
            unavailable(
                "WorkerAuthenticationUnavailable",
                "worker token authentication timed out",
            )
        })?
        .map_err(|_| {
            unavailable(
                "WorkerAuthenticationUnavailable",
                "worker token authentication is unavailable",
            )
        })?;
        authenticated
            .ok_or_else(|| unauthenticated("WorkerTokenInvalid", "invalid worker service token"))
    }

    /// Loads table access with a bounded wait; every error carries a typed reason.
    async fn access(&self, job: JobId) -> Result<TaskAccess, Status> {
        tokio::time::timeout(Duration::from_secs(30), self.access.access(job))
            .await
            .map_err(|_| {
                unavailable(
                    "WorkerTableAccessTimeout",
                    "worker storage access timed out",
                )
            })?
            .map_err(typed_access_error)
    }

    /// Records the expiry of credentials just delivered for `attempt`. Entries for
    /// already expired credentials are dropped: an absent entry only causes one
    /// extra refresh. No-credential hints expire after one lease, so vanished
    /// development workers cannot accumulate entries indefinitely. Credentials
    /// without a reported expiry are left absent and refreshed each heartbeat.
    fn remember_delivery(
        &self,
        attempt: AttemptId,
        credentials: Option<&wire::StorageCredentials>,
    ) {
        let now = Utc::now();
        let monotonic_now = Instant::now();
        let mut delivered = self
            .delivered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        delivered.retain(|_, entry| match entry {
            Delivered::NotVended(at) => {
                monotonic_now.saturating_duration_since(*at) < self.config.lease
            }
            Delivered::Expires(at) => *at > now,
        });
        match credentials.map(expiry) {
            None => {
                delivered.insert(attempt, Delivered::NotVended(monotonic_now));
            }
            Some(Some(at)) => {
                delivered.insert(attempt, Delivered::Expires(at));
            }
            Some(None) => {
                delivered.remove(&attempt);
            }
        }
    }

    fn forget_delivery(&self, attempt: AttemptId) {
        self.delivered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&attempt);
    }

    /// Credentials are refreshed only when the delivered ones expire within one
    /// lease. Because the lease spans several heartbeat intervals, a failed
    /// refresh is retried on later heartbeats before the credentials lapse.
    fn needs_refresh(&self, attempt: AttemptId) -> bool {
        let window = chrono::Duration::from_std(self.config.lease).unwrap_or(chrono::Duration::MAX);
        match self
            .delivered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&attempt)
        {
            None => true,
            Some(Delivered::NotVended(at)) => at.elapsed() >= self.config.lease,
            Some(Delivered::Expires(at)) => *at - Utc::now() <= window,
        }
    }

    /// Fetches fresh credentials for a renewed attempt. A failed refresh never
    /// fails the heartbeat: the renewal stands, and the worker keeps its current
    /// credentials. If those lapse, storage access fails with a storage
    /// authorization error that is classified as `RetryTask`.
    async fn refresh_credentials(&self, attempt: AttemptId) -> Option<wire::StorageCredentials> {
        let job = match self.attempt_job(attempt).await {
            Ok(job) => job,
            Err(error) => {
                tracing::warn!(
                    attempt_id = %attempt,
                    reason_code = reason_code(&database(error)),
                    "worker credential refresh skipped; lease renewal still returned"
                );
                return None;
            }
        };
        match self.access(job).await {
            Ok(access) => {
                self.remember_delivery(attempt, access.credentials.as_ref());
                access.credentials
            }
            Err(status) => {
                tracing::warn!(
                    attempt_id = %attempt,
                    reason_code = reason_code(&status),
                    "worker credential refresh failed; lease renewal still returned"
                );
                None
            }
        }
    }

    async fn attempt_job(&self, attempt: AttemptId) -> Result<JobId, PersistenceError> {
        let mut conn = self.pool.acquire().await?;
        let attempt = attempts::attempt(&mut conn, attempt).await?;
        Ok(plans::task(&mut conn, attempt.task_id).await?.job_id)
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
                return Err(precondition(
                    "TaskOfferConfigurationMismatch",
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
            return Err(rejection(
                tonic::Code::ResourceExhausted,
                "WorkerReadyQueueFull",
                "worker ready queue full",
                Disposition::RetrySameRequest,
            ));
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
            .ok_or_else(|| invalid_request("TaskOfferMissing", "missing task offers"))?;
        let mut conn = self.pool.acquire().await.map_err(database)?;
        jobs::job(&mut conn, first.organization, job)
            .await
            .map_err(database)?;
        let plan = planning::load_plan(&mut conn, job)
            .await
            .map_err(|_| invalid_stored_plan())?
            .ok_or_else(missing_stored_plan)?;
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
                return Err(invalid_request(
                    "TaskOfferOrganizationMismatch",
                    "task offer organization mismatch",
                ));
            }
            let task = tasks
                .get(&offer.task)
                .ok_or_else(|| precondition("TaskAbsentFromPlan", "task absent from plan"))?;
            let artifact = plan
                .artifact_of(task)
                .map_err(|_| precondition("TaskArtifactInvalid", "invalid task artifact"))?;
            let digest = artifact_digest(&artifact);
            if offer.artifact.sha256 != digest.as_bytes() {
                return Err(invalid_request(
                    "TaskArtifactDigestMismatch",
                    "task artifact digest mismatch",
                ));
            }
            expected.push((offer, digest));
        }
        let access = self.access(job).await?;
        for (offer, digest) in expected {
            let location = Location::parse(&offer.artifact.uri)
                .map_err(|_| invalid_request("TaskArtifactUriInvalid", "invalid artifact URI"))?;
            let bytes = tokio::time::timeout(Duration::from_secs(30), access.store.read(&location))
                .await
                .map_err(|_| {
                    unavailable("TaskArtifactReadTimeout", "task artifact read timed out")
                })?
                .map_err(|_| unavailable("TaskArtifactUnavailable", "task artifact unavailable"))?;
            let actual = bergsmith_protocol::artifact::TaskArtifact::decode(bytes)
                .map_err(|_| invalid_request("TaskArtifactInvalid", "invalid task artifact"))?;
            if artifact_digest(&actual) != digest {
                return Err(invalid_request(
                    "TaskArtifactContentMismatch",
                    "task artifact content mismatch",
                ));
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
        let mut grant = wire::TaskGrant::decode(poll.grant_bytes.as_deref().ok_or_else(|| {
            rejection(
                tonic::Code::Internal,
                "StoredGrantMissing",
                "missing stored grant",
                Disposition::PermanentFailure,
            )
        })?)
        .map_err(|_| {
            rejection(
                tonic::Code::Internal,
                "StoredGrantInvalid",
                "invalid stored grant",
                Disposition::PermanentFailure,
            )
        })?;
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
        drop(conn);
        grant.lease_expires_at = Some(timestamp(attempt.lease_expires_at));
        self.remember_delivery(attempt_id, access.credentials.as_ref());
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
            .map_err(|_| invalid_stored_plan())?
            .ok_or_else(missing_stored_plan)?;
        // The attempt may only use what is left of the job's execution budget.
        // A spent budget grants nothing: the scheduler stops the job instead.
        let remaining = jobs::remaining_execution_budget(&mut conn, task.job_id)
            .await
            .map_err(database)?;
        let (limits, lease) = match remaining {
            Some(remaining) if remaining.is_zero() => {
                reservation.consumed = true;
                return Ok(None);
            }
            Some(remaining) => (
                capped_limits(offer.limits, remaining),
                // The lease never outlasts the budget; the persisted attempt
                // caps it again against the deadline, so the two cannot
                // disagree.
                self.config.lease.min(remaining),
            ),
            // A job without a deadline is not bounded.
            None => (offer.limits, self.config.lease),
        };
        let (attempt_id, grant) = draft_grant(offer, &task, &plan, session, limits)?;
        let new = attempts::NewGrant {
            attempt_id,
            task_id: task.id,
            worker_session_id: session,
            lease,
        };
        let payload = ledger::GrantPayload {
            grant_bytes: &grant.encode_to_vec(),
            output_prefix: &grant.output_prefix,
            envelope_bytes: &offer.limits.encode_to_vec(),
            generation: grant.generation,
        };
        let granted = if let Some(limits) = self.admission.get() {
            if !self.dispatch_ready.load(Ordering::Acquire) {
                return Err(scheduler_not_ready());
            }
            ledger::grant_admitted(
                &mut conn, identity, &new, request, self.epoch, &payload, *limits,
            )
            .await
        } else {
            ledger::grant(&mut conn, identity, &new, request, self.epoch, &payload).await
        };
        match granted {
            Ok(poll) => {
                reservation.consumed = poll.attempt_id == Some(*attempt_id.as_uuid());
                Ok(Some(poll))
            }
            // The task changed after the unlocked read; nothing was persisted.
            // Keep the offer so the caller redrafts it from a fresh read.
            Err(PersistenceError::StaleAttempt { attempt }) if attempt == attempt_id => Ok(None),
            // The job's execution budget ran out between the check and the
            // write. The scheduler stops the job; no attempt is started.
            Err(PersistenceError::ExecutionBudgetExceeded { .. }) => {
                reservation.consumed = true;
                Ok(None)
            }
            Err(error) if offer_unusable(&error) => {
                tracing::warn!(
                    task_id = %offer.task,
                    reason_code = reason_code(&database(error)),
                    "dropping ungrantable task offer"
                );
                reservation.consumed = true;
                Ok(None)
            }
            Err(error) => Err(database(error)),
        }
    }

    async fn pull(
        &self,
        identity: &ledger::Identity,
        session_id: WorkerSessionId,
        request: Uuid,
    ) -> Result<wire::RequestTaskResponse, Status> {
        let deadline = Instant::now() + self.config.long_poll_timeout;
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
                .map_err(|_| {
                    rejection(
                        tonic::Code::DeadlineExceeded,
                        "TaskGrantDecisionTimeout",
                        "task grant decision timed out",
                        Disposition::RetrySameRequest,
                    )
                })??;
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

/// Caps a task's duration with the time left in the job's execution budget.
///
/// The envelope's own limit stays authoritative: the budget may only shorten
/// an attempt, never lengthen it. Nanoseconds are preserved, so a positive
/// budget below one second never becomes a zero duration that the envelope
/// validation and the worker both reject.
pub(crate) fn capped_duration(
    current: Option<prost_types::Duration>,
    remaining: Duration,
) -> prost_types::Duration {
    let remaining = prost_duration(remaining);
    match current {
        Some(current)
            if (current.seconds, current.nanos) < (remaining.seconds, remaining.nanos) =>
        {
            current
        }
        _ => remaining,
    }
}

/// A standard duration as the protocol carries it.
fn prost_duration(duration: Duration) -> prost_types::Duration {
    prost_types::Duration {
        seconds: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        nanos: i32::try_from(duration.subsec_nanos()).unwrap_or(0),
    }
}

/// Caps a task envelope with the time left in the job's execution budget.
fn capped_limits(
    mut limits: bergsmith_protocol::cluster::ResourceLimits,
    remaining: Duration,
) -> bergsmith_protocol::cluster::ResourceLimits {
    limits.max_duration = Some(capped_duration(limits.max_duration, remaining));
    limits
}

fn draft_grant(
    offer: &TaskOffer,
    task: &plans::Task,
    plan: &bergsmith_maintenance::PlanDocument,
    session: WorkerSessionId,
    limits: bergsmith_protocol::cluster::ResourceLimits,
) -> Result<(AttemptId, wire::TaskGrant), Status> {
    let attempt_id = AttemptId::generate();
    let prefix = Location::parse(plan.baseline().data_location())
        .map_err(|_| {
            precondition(
                "PlannedDataLocationInvalid",
                "invalid planned data location",
            )
        })?
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
            input_state_id: plan.sha256().map_err(|_| invalid_stored_plan())?.to_hex(),
            capability_profile: offer.capability.clone(),
            output_prefix: prefix.to_string(),
            limits: Some(limits),
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
        let identity = self.authenticate(&request).await?;
        let registration = request.into_inner();
        if registration.pool != identity.pool {
            return Err(rejection(
                tonic::Code::PermissionDenied,
                "WorkerPoolMismatch",
                "worker pool mismatch",
                Disposition::PermanentFailure,
            ));
        }
        if registration.protocol_revision < 2 {
            return Err(precondition(
                "WorkerProtocolUnsupported",
                "unsupported worker protocol",
            ));
        }
        let capacity = registration
            .capacity
            .ok_or_else(|| invalid_request("WorkerCapacityMissing", "missing worker capacity"))?;
        if capacity.max_concurrent_tasks != 1 {
            return Err(invalid_request(
                "WorkerConcurrencyUnsupported",
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
                memory_bytes: i64::try_from(capacity.memory_bytes).map_err(|_| {
                    invalid_request("WorkerCapacityInvalid", "invalid worker memory")
                })?,
                cpu_millis: i32::try_from(capacity.cpu_millis)
                    .map_err(|_| invalid_request("WorkerCapacityInvalid", "invalid worker CPU"))?,
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
        let identity = self.authenticate(&request).await?;
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
        let identity = self.authenticate(&request).await?;
        let heartbeat = request.into_inner();
        let session_id = id(&heartbeat.worker_session_id)?;
        if heartbeat.attempts.len() > 1 {
            return Err(invalid_request(
                "TooManyExecutingAttempts",
                "too many executing attempts",
            ));
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
            drop(conn);
            // The renewal is committed: from here on nothing may discard it.
            let credentials = if !matches!(renewal, ledger::Renewal::Renewed(_)) {
                self.forget_delivery(attempt_id);
                None
            } else if self.needs_refresh(attempt_id) {
                self.refresh_credentials(attempt_id).await
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
        let identity = self.authenticate(&request).await?;
        let report = request.into_inner();
        let receipt = report
            .receipt
            .ok_or_else(|| invalid_request("TaskReceiptMissing", "missing receipt"))?;
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
            .map_err(|_| invalid_stored_plan())?
            .ok_or_else(missing_stored_plan)?;
        drop(conn);
        self.forget_delivery(attempt_id);
        let access = self.access(task.job_id).await?;
        let verified =
            match results::verify_receipt(&plan, &canonical, &access.store, &access.metadata).await
            {
                Ok(verified) => verified,
                Err(error) if invalid_output(&error) => return Ok(invalid()),
                Err(_) => return Err(verification_unavailable()),
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
        let identity = self.authenticate(&request).await?;
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
        let identity = self.authenticate(&request).await?;
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
        let identity = self.authenticate(&request).await?;
        let report = request.into_inner();
        let session = id(&report.worker_session_id)?;
        let attempt_id = id(&report.attempt_id)?;
        let failure = report
            .failure
            .ok_or_else(|| invalid_request("TaskFailureMissing", "missing failure"))?;
        let mut conn = self.pool.acquire().await.map_err(database)?;
        let attempt = ledger::owned_attempt(&mut conn, &identity, session, attempt_id)
            .await
            .map_err(database)?;
        if report.generation != attempt.generation {
            return Err(precondition(
                "StaleAttemptGeneration",
                "stale attempt generation",
            ));
        }
        self.forget_delivery(attempt_id);
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
        return Err(invalid_request("TaskOfferInvalid", "invalid task offer"));
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

    fn failure(status: &Status) -> (String, i32) {
        let failure = bergsmith_protocol::common::Failure::decode(status.details()).unwrap();
        (failure.reason_code, failure.disposition)
    }

    #[test]
    fn access_errors_gain_a_typed_reason_without_losing_an_existing_one() {
        let retry = i32::from(Disposition::RetrySameRequest);
        let permanent = i32::from(Disposition::PermanentFailure);
        for code in [
            tonic::Code::Unavailable,
            tonic::Code::DeadlineExceeded,
            tonic::Code::ResourceExhausted,
            tonic::Code::Aborted,
        ] {
            let status = typed_access_error(Status::new(code, "catalog unavailable"));
            assert_eq!(status.code(), code);
            assert_eq!(status.message(), "catalog unavailable");
            assert_eq!(
                failure(&status),
                ("WorkerTableAccessUnavailable".into(), retry)
            );
        }
        let status = typed_access_error(Status::failed_precondition("missing binding"));
        assert_eq!(
            failure(&status),
            ("WorkerTableAccessRejected".into(), permanent)
        );
        let typed = typed_access_error(precondition("ExecutionAuthorityRevoked", "revoked"));
        assert_eq!(
            failure(&typed),
            ("ExecutionAuthorityRevoked".into(), permanent)
        );
        assert_eq!(
            failure(&verification_unavailable()),
            ("TaskResultVerificationUnavailable".into(), retry)
        );
        assert!(reason_code(&Status::internal("untyped")).is_empty());
    }

    #[test]
    fn only_offer_scoped_grant_errors_drop_the_offer() {
        let task = TaskId::generate();
        let job = JobId::generate();
        for error in [
            PersistenceError::RetryBudgetExhausted { task },
            PersistenceError::JobNotInState {
                job,
                state: JobState::Succeeded,
                operation: "grant an attempt",
            },
            PersistenceError::TaskTransition(bergsmith_common::task::TaskTransitionError {
                from: bergsmith_common::task::TaskState::Accepted,
                to: bergsmith_common::task::TaskState::Leased,
            }),
            PersistenceError::AlreadyExists {
                entity: "attempt",
                constraint: "attempts_one_active".into(),
            },
            PersistenceError::NotFound {
                entity: "task",
                id: task.to_string(),
            },
            PersistenceError::InvalidValue {
                entity: "task resource envelope",
                constraint: "task_resource_envelope_is_immutable".into(),
            },
        ] {
            assert!(offer_unusable(&error), "{error:?}");
        }
        for error in [
            PersistenceError::LeadershipLost { epoch: 1 },
            PersistenceError::AdmissionUnavailable,
            PersistenceError::Database(sqlx::Error::PoolTimedOut),
            PersistenceError::InvalidValue {
                entity: "worker session",
                constraint: "worker_session_authority".into(),
            },
            PersistenceError::NotFound {
                entity: "attempt",
                id: String::new(),
            },
            PersistenceError::StaleAttempt {
                attempt: AttemptId::generate(),
            },
        ] {
            assert!(!offer_unusable(&error), "{error:?}");
        }
    }

    #[tokio::test]
    async fn credentials_are_refreshed_only_within_one_lease_of_expiry() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/refresh_test")
            .unwrap();
        let service =
            WorkerService::new(pool, 1, WorkerConfig::default(), Arc::new(NoAccess)).unwrap();
        let credentials = |seconds: i64| wire::StorageCredentials {
            prefix: "s3://bucket/table".into(),
            properties: BTreeMap::new(),
            expires_at: Some(prost_types::Timestamp {
                seconds: (Utc::now() + chrono::Duration::seconds(seconds)).timestamp(),
                nanos: 0,
            }),
        };
        let attempt = AttemptId::generate();
        assert!(service.needs_refresh(attempt), "unknown delivery");
        service.remember_delivery(attempt, None);
        assert!(!service.needs_refresh(attempt), "nothing is vended");
        service.remember_delivery(attempt, Some(&credentials(3600)));
        assert!(!service.needs_refresh(attempt));
        service.remember_delivery(attempt, Some(&credentials(60)));
        assert!(service.needs_refresh(attempt), "expires within the lease");
        let mut unbounded = credentials(3600);
        unbounded.expires_at = None;
        service.remember_delivery(attempt, Some(&unbounded));
        assert!(service.needs_refresh(attempt), "unknown expiry");
        let expired = AttemptId::generate();
        service.remember_delivery(expired, Some(&credentials(-1)));
        service.remember_delivery(attempt, Some(&credentials(3600)));
        assert!(!service.delivered.lock().unwrap().contains_key(&expired));
        service.forget_delivery(attempt);
        assert!(service.needs_refresh(attempt));
    }

    #[tokio::test(start_paused = true)]
    async fn vanished_workers_without_credentials_do_not_accumulate_delivery_hints() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/refresh_test")
            .unwrap();
        let service =
            WorkerService::new(pool, 1, WorkerConfig::default(), Arc::new(NoAccess)).unwrap();
        let abandoned = AttemptId::generate();
        service.remember_delivery(abandoned, None);
        assert!(!service.needs_refresh(abandoned));

        // Advance the monotonic cache clock without sleeping or changing a
        // durable lease. Losing this optimization must only cause a refresh.
        tokio::time::advance(service.config.lease).await;
        assert!(service.needs_refresh(abandoned));

        let live = AttemptId::generate();
        service.remember_delivery(live, None);
        let delivered = service.delivered.lock().unwrap();
        assert!(!delivered.contains_key(&abandoned));
        assert!(delivered.contains_key(&live));
        assert_eq!(delivered.len(), 1);
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
