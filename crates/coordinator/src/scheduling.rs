//! Bounded durable-state scheduling, admission, and startup/periodic reconciliation.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bergsmith_common::ids::{JobId, OrganizationId, TaskId};
use bergsmith_common::job::JobState;
use bergsmith_maintenance::PlanDocument;
use bergsmith_persistence::{PersistenceError, publications, scheduling as ledger};
use prost::Message;
use sqlx::PgPool;
use tonic::Status;
use uuid::Uuid;

use crate::{
    planning,
    workers::{TaskOffer, WorkerService},
};

pub use ledger::{AdmissionLimits, RetryPolicy};

/// Read-only validation of the original plan. No variant authorizes a new baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eligibility {
    /// Original baseline, input retention, and authority remain valid.
    Valid,
    /// Original requirements or retained planning metadata changed.
    ReplanRequired(&'static str),
    /// Unchanged-baseline source integrity or execution authority failed.
    PermanentFailure(&'static str),
}

/// Coordinator-owned catalog/metadata access, never implemented by a worker.
#[async_trait]
pub trait SchedulingAccess: Send + Sync + 'static {
    /// Checks catalog state and input metadata, without reading table rows.
    ///
    /// # Errors
    /// Transient catalog/storage failures leave execution and retry timers unchanged.
    async fn validate(
        &self,
        job: &bergsmith_persistence::jobs::Job,
        plan: &PlanDocument,
    ) -> Result<Eligibility, Status>;

    /// Resolves retained publication evidence without replaying a mutation.
    /// Every durable write must be fenced by `epoch`.
    ///
    /// # Errors
    /// Missing catalog access/evidence must retain the publication barrier.
    async fn recover_publication(
        &self,
        pool: &PgPool,
        job: &bergsmith_persistence::jobs::Job,
        epoch: i64,
    ) -> Result<(), Status>;
}

/// Scheduling defaults independent of immutable planner and task execution limits.
#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    /// Global, tenant, pool, and execution reservation limits.
    pub admission: AdmissionLimits,
    /// Retry policy captured when dispatch configuration is first registered.
    pub retry: RetryPolicy,
    /// Maximum original plan input bytes admitted as one job.
    pub max_job_input_bytes: u64,
    /// Maximum original estimated job duration in milliseconds.
    pub max_job_duration_millis: u64,
    /// Maximum number of job rows read in one database page.
    pub page_size: i64,
    /// Repair/refill interval; notifications are unnecessary for correctness.
    pub interval: Duration,
    /// Deadline for one job's external validation or publication recovery.
    pub validation_timeout: Duration,
    /// How long after send authorization periodic passes leave a `Committing`
    /// job to its live publisher. It must exceed one commit request's connect
    /// plus request timeout and the time to record its outcome. The first
    /// pass of a scheduler recovers every publication regardless.
    pub publication_send_grace: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            admission: AdmissionLimits::default(),
            retry: RetryPolicy::default(),
            max_job_input_bytes: 64 * 1024 * 1024 * 1024,
            max_job_duration_millis: 180_000,
            page_size: 128,
            interval: Duration::from_secs(1),
            validation_timeout: Duration::from_secs(30),
            publication_send_grace: Duration::from_secs(120),
        }
    }
}

/// Errors retain a typed category and never contain credentials.
#[derive(Debug, thiserror::Error)]
pub enum SchedulingError {
    /// Invalid local configuration.
    #[error("invalid scheduler configuration")]
    Configuration,
    /// Durable scheduling state is unavailable or inconsistent.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// Stored immutable plan could not be verified.
    #[error(transparent)]
    Plan(#[from] planning::PlanningError),
    /// Catalog, storage, or safe offer validation could not complete.
    #[error("scheduler external validation unavailable")]
    Access(#[source] Status),
    /// A bounded external operation exceeded its deadline.
    #[error("scheduler external validation timed out")]
    Timeout,
}

impl SchedulingError {
    /// Stable reason for operator logs and service error translation.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Configuration => "InvalidSchedulerConfiguration",
            Self::Persistence(_) => "SchedulerLedgerUnavailable",
            Self::Plan(_) => "SchedulerStoredPlanInvalid",
            Self::Access(_) => "SchedulerAccessUnavailable",
            Self::Timeout => "SchedulerValidationTimedOut",
        }
    }

    /// Whether the failure concerns the coordinator itself (leadership or the
    /// database) rather than one job. Only these close dispatch; a job-scoped
    /// failure skips that job and the pass continues.
    #[must_use]
    pub const fn is_infrastructure(&self) -> bool {
        matches!(
            self,
            Self::Persistence(
                PersistenceError::LeadershipLost { .. }
                    | PersistenceError::Database(_)
                    | PersistenceError::Migration(_)
            )
        )
    }
}

/// One active coordinator's repair loop. Serve only [`Self::worker_service`].
/// A fresh instance starts with dispatch closed until a complete pass succeeds.
pub struct Scheduler {
    pool: PgPool,
    epoch: i64,
    service: WorkerService,
    access: Arc<dyn SchedulingAccess>,
    config: SchedulerConfig,
    pass: tokio::sync::Mutex<()>,
    cursor: std::sync::Mutex<Uuid>,
    started: AtomicBool,
    unresolved: AtomicI64,
}

// Abort/drop must close dispatch as reliably as an explicit shutdown signal.
struct RunningScheduler(WorkerService);

impl Drop for RunningScheduler {
    fn drop(&mut self) {
        self.0.set_dispatch_ready(false);
    }
}

impl Scheduler {
    /// Wraps a fresh worker service with durable admission and startup fencing.
    /// Database and leadership epoch come from the service's original context.
    ///
    /// # Errors
    /// Rejects invalid budgets, page size, retry bounds, interval/deadline/grace,
    /// or a worker service that already belongs to a scheduler.
    pub fn new(
        service: WorkerService,
        access: Arc<dyn SchedulingAccess>,
        config: SchedulerConfig,
    ) -> Result<Self, SchedulingError> {
        if !config.admission.is_valid()
            || !config.retry.is_valid()
            || config.max_job_input_bytes == 0
            || config.max_job_duration_millis == 0
            || !(1..=10_000).contains(&config.page_size)
            || config.interval.is_zero()
            || config.interval > Duration::from_secs(86_400)
            || config.validation_timeout.is_zero()
            || config.validation_timeout > Duration::from_secs(3_600)
            || config.publication_send_grace > Duration::from_secs(86_400)
        {
            return Err(SchedulingError::Configuration);
        }
        let (pool, epoch) = service.scheduler_context();
        Ok(Self {
            pool,
            epoch,
            service: service
                .managed(config.admission)
                .map_err(SchedulingError::Access)?,
            access,
            config,
            pass: tokio::sync::Mutex::new(()),
            cursor: std::sync::Mutex::new(Uuid::nil()),
            started: AtomicBool::new(false),
            unresolved: AtomicI64::new(0),
        })
    }

    /// The managed RPC service; registration/heartbeats/report recovery remain usable during startup.
    #[must_use]
    pub fn worker_service(&self) -> WorkerService {
        self.service.clone()
    }

    /// Jobs in `CommitOutcomeUnknown` at the end of the last pass. Each keeps
    /// its table barrier and admission slots until evidence or an operator
    /// resolves it, so a growing value means admission capacity is shrinking.
    #[must_use]
    pub fn unresolved_publications(&self) -> i64 {
        self.unresolved.load(Ordering::Acquire)
    }

    /// Registers safe, complete dispatch configuration without admitting stale work.
    /// Exact registration retries preserve all resource limits and backoff policy.
    ///
    /// # Errors
    /// Rejects wrong jobs, invalid artifacts, changed configuration, or database failure.
    pub async fn register(&self, job: JobId, offers: &[TaskOffer]) -> Result<(), SchedulingError> {
        let Some(first) = offers.first() else {
            return Err(SchedulingError::Configuration);
        };
        if offers
            .iter()
            .any(|offer| offer.organization != first.organization)
        {
            return Err(SchedulingError::Configuration);
        }
        let specs: Vec<_> = offers
            .iter()
            .map(dispatch_spec)
            .collect::<Result<_, _>>()
            .map_err(SchedulingError::Access)?;
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        bergsmith_persistence::jobs::job(&mut conn, first.organization, job).await?;
        if let Some(registration) = ledger::dispatch_registration(&mut conn, job).await? {
            return if registration.matches(&specs, self.config.retry) {
                Ok(())
            } else {
                Err(SchedulingError::Configuration)
            };
        }
        drop(conn);
        self.service
            .validate_offers(job, offers)
            .await
            .map_err(SchedulingError::Access)?;
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        ledger::register_dispatch(&mut conn, job, &specs, self.config.retry).await?;
        Ok(())
    }

    /// Performs startup/periodic recovery before rebuilding the bounded ready cache.
    /// No database transaction is held across catalog/storage or RPC work.
    ///
    /// A failure scoped to one job (its catalog, storage, binding, plan, or a
    /// concurrent state change) skips that job with a logged reason code; the
    /// pass continues for every other job and table.
    ///
    /// # Errors
    /// Lost leadership or database failure closes dispatch. Already accepted
    /// results and authorized publications remain durable; a later pass
    /// resumes read-only reconciliation.
    pub async fn reconcile(&self) -> Result<(), SchedulingError> {
        let _pass = self.pass.lock().await;
        let result = self.reconcile_inner().await;
        self.service.set_dispatch_ready(result.is_ok());
        result
    }

    async fn reconcile_inner(&self) -> Result<(), SchedulingError> {
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        ledger::check_leadership(&mut conn, self.epoch).await?;
        // The first pass recovers every publication. Later passes leave a
        // recently authorized send to the live publisher awaiting its response.
        let publications = if self.started.load(Ordering::Acquire) {
            publications::settled_recoverable_jobs(&mut conn, self.config.publication_send_grace)
                .await?
        } else {
            publications::recoverable_jobs(&mut conn).await?
        };
        drop(conn);
        for job in publications {
            let recovered = tokio::time::timeout(
                self.config.validation_timeout,
                self.access
                    .recover_publication(&self.pool, &job, self.epoch),
            )
            .await
            .map_err(|_| SchedulingError::Timeout)
            .and_then(|result| result.map_err(SchedulingError::Access));
            if let Err(error) = recovered {
                tracing::warn!(
                    job_id = %job.id,
                    reason_code = error.reason_code(),
                    "publication recovery deferred; barrier and outputs retained"
                );
            }
        }
        let mut after = Uuid::nil();
        loop {
            let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
            let jobs = ledger::active_jobs(&mut conn, after, self.config.page_size).await?;
            drop(conn);
            if jobs.is_empty() {
                break;
            }
            for job in jobs {
                after = *job.id.as_uuid();
                if let Err(error) = self.reconcile_job(&job).await {
                    if error.is_infrastructure() {
                        return Err(error);
                    }
                    tracing::warn!(
                        job_id = %job.id,
                        reason_code = error.reason_code(),
                        "job reconciliation skipped; execution state and retry timers unchanged"
                    );
                }
            }
        }
        self.refill().await?;
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        self.observe_unresolved(publications::unresolved_count(&mut conn).await?);
        ledger::check_leadership(&mut conn, self.epoch).await?;
        self.started.store(true, Ordering::Release);
        Ok(())
    }

    fn observe_unresolved(&self, count: i64) {
        let previous = self.unresolved.swap(count, Ordering::AcqRel);
        if count != previous && count > 0 {
            tracing::warn!(
                unresolved_publications = count,
                "publication outcomes unresolved; their table barriers and admission slots stay reserved until evidence or operator attestation resolves them"
            );
        }
    }

    async fn refill(&self) -> Result<(), SchedulingError> {
        let slots = self.service.reset_ready_queue();
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let cursor = *self
            .cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tasks =
            ledger::ready_after(&mut conn, cursor, i64::try_from(slots).unwrap_or(i64::MAX))
                .await?;
        if let Some((last, _)) = tasks.last() {
            *self
                .cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = *last;
        }
        let mut offers = Vec::with_capacity(tasks.len());
        for (task, organization) in tasks {
            let offer = ledger::dispatch_spec(&mut conn, TaskId::from_uuid(task))
                .await?
                .ok_or(SchedulingError::Configuration)
                .and_then(|spec| task_offer(OrganizationId::from_uuid(organization), spec));
            match offer {
                Ok(offer) => offers.push(offer),
                Err(error) => tracing::warn!(
                    task_id = %task,
                    reason_code = error.reason_code(),
                    "task skipped by ready-cache refill"
                ),
            }
        }
        drop(conn);
        for offer in offers {
            let task = offer.task;
            if let Err(status) = self.service.enqueue_registered(offer).await {
                tracing::warn!(
                    task_id = %task,
                    reason_code = SchedulingError::Access(status).reason_code(),
                    "task skipped by ready-cache refill"
                );
            }
        }
        Ok(())
    }

    async fn reconcile_job(
        &self,
        job: &bergsmith_persistence::jobs::Job,
    ) -> Result<(), SchedulingError> {
        if job.state == JobState::Cancelling && job.publication_started_at.is_none() {
            let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
            ledger::stop(
                &mut conn,
                job.id,
                job.state_revision,
                self.epoch,
                JobState::Cancelled,
                "CancellationRequested",
            )
            .await?;
            return Ok(());
        }
        if !matches!(job.state, JobState::Queued | JobState::Running) {
            return Ok(());
        }
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        // Revocation is checked before anything else needs dispatch
        // configuration, so a revoked requester is failed even when its plan
        // has no registered dispatch yet.
        let current =
            bergsmith_persistence::jobs::job(&mut conn, job.organization_id, job.id).await?;
        if self.revoke_if_unauthorized(&mut conn, &current).await? {
            return Ok(());
        }
        let plan = planning::load_plan(&mut conn, job.id)
            .await?
            .ok_or(SchedulingError::Configuration)?;
        let table_uuid: Uuid =
            sqlx::query_scalar("SELECT table_uuid FROM table_identities WHERE id=$1")
                .bind(job.table_id.as_uuid())
                .fetch_one(&mut *conn)
                .await
                .map_err(PersistenceError::from)?;
        if table_uuid != plan.baseline().table_uuid() {
            ledger::stop(
                &mut conn,
                job.id,
                job.state_revision,
                self.epoch,
                JobState::ReplanRequired,
                "PlanningTableIdentityMismatch",
            )
            .await?;
            return Ok(());
        }
        let tasks = bergsmith_persistence::plans::tasks(&mut conn, plan.plan_id()).await?;
        let registered = if let Some(task) = tasks.first() {
            ledger::dispatch_spec(&mut conn, task.id).await?.is_some()
        } else {
            false
        };
        if !registered {
            return Ok(());
        }
        // Fencing expired or session-lost attempts needs no external validation;
        // only releasing their retries does.
        ledger::repair_attempts(&mut conn, job.id, self.epoch).await?;
        let current =
            bergsmith_persistence::jobs::job(&mut conn, job.organization_id, job.id).await?;
        let need = ledger::validation_need(&mut conn, &current, self.config.admission).await?;
        if need == ledger::ValidationNeed::None {
            return Ok(());
        }
        if need == ledger::ValidationNeed::Admission
            && (plan.estimate().bytes > self.config.max_job_input_bytes
                || plan.estimate().duration_millis > self.config.max_job_duration_millis)
        {
            ledger::stop(
                &mut conn,
                current.id,
                current.state_revision,
                self.epoch,
                JobState::Failed,
                "AdmissionJobBudgetExceeded",
            )
            .await?;
            return Ok(());
        }
        drop(conn);
        self.validate_and_apply(&current, &plan).await
    }

    /// Fails a job whose requester no longer holds the submitting grant.
    ///
    /// Returns whether the job was stopped. Revocation is job-scoped and is
    /// evaluated even while budgets or the table are busy; worker sessions and
    /// tokens are untouched.
    async fn revoke_if_unauthorized(
        &self,
        conn: &mut sqlx::PgConnection,
        job: &bergsmith_persistence::jobs::Job,
    ) -> Result<bool, SchedulingError> {
        match bergsmith_persistence::grants::require_job_permission(
            conn,
            job,
            bergsmith_common::permission::Permission::SubmitJobs,
        )
        .await
        {
            Ok(()) => Ok(false),
            Err(PersistenceError::PermissionDenied { .. }) => {
                record_admission_decision(
                    conn,
                    job,
                    bergsmith_common::audit::AuditAction::JobAdmissionDenied,
                    bergsmith_common::audit::AuditOutcome::Denied,
                    "AuthorizationRevoked",
                )
                .await;
                Ok(ledger::stop(
                    conn,
                    job.id,
                    job.state_revision,
                    self.epoch,
                    JobState::Failed,
                    "AuthorizationRevoked",
                )
                .await?)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Validates a job that needs admission or retry release, then applies
    /// the outcome against the revision the decision was based on.
    async fn validate_and_apply(
        &self,
        current: &bergsmith_persistence::jobs::Job,
        plan: &PlanDocument,
    ) -> Result<(), SchedulingError> {
        let eligibility = tokio::time::timeout(
            self.config.validation_timeout,
            self.access.validate(current, plan),
        )
        .await
        .map_err(|_| SchedulingError::Timeout)?
        .map_err(SchedulingError::Access)?;
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let stopped = match eligibility {
            Eligibility::ReplanRequired(reason) => (JobState::ReplanRequired, reason),
            Eligibility::PermanentFailure(reason) => (JobState::Failed, reason),
            Eligibility::Valid => {
                ledger::reconcile_execution(&mut conn, current.id, self.epoch).await?;
                let latest = bergsmith_persistence::jobs::job(
                    &mut conn,
                    current.organization_id,
                    current.id,
                )
                .await?;
                let admitted = matches!(latest.state, JobState::Queued | JobState::Running)
                    && latest.cancellation.is_none()
                    && ledger::admit(&mut conn, current.id, self.epoch, self.config.admission)
                        .await?;
                if admitted {
                    record_admission_decision(
                        &mut conn,
                        &latest,
                        bergsmith_common::audit::AuditAction::JobAdmitted,
                        bergsmith_common::audit::AuditOutcome::Allowed,
                        "Admitted",
                    )
                    .await;
                }
                return Ok(());
            }
        };
        // The decision is based on `current`; a job that moved meanwhile
        // (publication, cancellation) is left to the next pass.
        if !ledger::stop(
            &mut conn,
            current.id,
            current.state_revision,
            self.epoch,
            stopped.0,
            stopped.1,
        )
        .await?
        {
            tracing::debug!(job_id = %current.id, "validation outcome superseded by a newer job state");
        }
        Ok(())
    }

    /// Runs reconciliation immediately and periodically until shutdown.
    /// Failed passes fence dispatch and retry at the configured interval.
    pub async fn run(&self, shutdown: impl Future<Output = ()> + Send) {
        let _running = RunningScheduler(self.service.clone());
        tokio::pin!(shutdown);
        let mut interval = tokio::time::interval(self.config.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = &mut shutdown => { self.service.set_dispatch_ready(false); return; },
                _ = interval.tick() => {
                    tokio::select! {
                        () = &mut shutdown => { self.service.set_dispatch_ready(false); return; },
                        result = self.reconcile() => {
                            if let Err(error) = result {
                                tracing::warn!(reason_code=error.reason_code(), "scheduler reconciliation failed; dispatch fenced");
                            }
                        }
                    }
                }
            }
        }
    }
}

pub(crate) fn dispatch_spec(offer: &TaskOffer) -> Result<ledger::DispatchSpec, Status> {
    Ok(ledger::DispatchSpec {
        task_id: *offer.task.as_uuid(),
        pool: offer.pool.clone(),
        capability: offer.capability.clone(),
        artifact_uri: offer.artifact.uri.clone(),
        artifact_sha256: offer.artifact.sha256.clone(),
        envelope_bytes: offer.limits.encode_to_vec(),
        memory_bytes: i64::try_from(offer.limits.memory_bytes)
            .map_err(|_| Status::invalid_argument("invalid task memory"))?,
        cpu_millis: i32::try_from(offer.limits.cpu_millis)
            .map_err(|_| Status::invalid_argument("invalid task CPU"))?,
    })
}

fn task_offer(
    organization: OrganizationId,
    spec: ledger::DispatchSpec,
) -> Result<TaskOffer, SchedulingError> {
    let limits =
        bergsmith_protocol::cluster::ResourceLimits::decode(spec.envelope_bytes.as_slice())
            .map_err(|_| SchedulingError::Configuration)?;
    if i64::try_from(limits.memory_bytes).ok() != Some(spec.memory_bytes)
        || i32::try_from(limits.cpu_millis).ok() != Some(spec.cpu_millis)
    {
        return Err(SchedulingError::Configuration);
    }
    Ok(TaskOffer {
        organization,
        task: TaskId::from_uuid(spec.task_id),
        pool: spec.pool,
        capability: spec.capability,
        artifact: bergsmith_protocol::cluster::ArtifactRef {
            uri: spec.artifact_uri,
            sha256: spec.artifact_sha256,
        },
        limits,
    })
}

/// Records an admission decision. Audit failure is logged but never changes the
/// scheduling outcome: the decision is already durable in the job ledger.
async fn record_admission_decision(
    conn: &mut sqlx::PgConnection,
    job: &bergsmith_persistence::jobs::Job,
    action: bergsmith_common::audit::AuditAction,
    outcome: bergsmith_common::audit::AuditOutcome,
    reason: &str,
) {
    if let Err(error) = crate::identity::audit::job_decision(
        conn,
        job.organization_id,
        Some(job.created_by),
        action,
        &job.id.to_string(),
        outcome,
        Some(reason),
    )
    .await
    {
        tracing::warn!(
            reason = error.reason_code(),
            job_id = %job.id,
            "admission audit record failed"
        );
    }
}
