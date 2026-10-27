//! The planning and dispatch-registration loop (BS-020.T1).
//!
//! Submission and schedule admission create a durable `Planning` job. This
//! loop turns that row into an immutable plan: it resolves the table through
//! the catalog, plans with the base configuration (or the policy that admitted
//! a scheduled run), records the outcome, writes the task artifacts, and
//! registers their dispatch envelopes with the scheduler.
//!
//! A failure that an operator must fix ends the job with a typed reason. A
//! transient catalog, storage, or database failure leaves the job `Planning`
//! and retries with bounded backoff; no partial plan is visible.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bergsmith_common::ids::{JobId, PlanId};
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_maintenance::orphans::{
    OrphanError, OrphanPlanDocument, OrphanPolicy, ReachabilityBounds, Reachable, select_orphans,
};
use bergsmith_maintenance::{
    ExpirationError, ExpirationPlanDocument, ExpirationPolicy, PlanDocument, PlanError, PlanIds,
    PlanInput, PlanOutcome, PlannedTask, Planner, PlannerConfig, plan_expiration,
};
use bergsmith_persistence::expirations::{self, NewExpirationPlan};
use bergsmith_persistence::orphans::{self as orphan_plans, NewOrphanPlan};
use bergsmith_persistence::scheduling::RetryPolicy;
use bergsmith_persistence::{PersistenceError, jobs, schedules};
use bergsmith_protocol::hashing::artifact_digest;
use bergsmith_storage::Location;
use chrono::Utc;
use prost::Message;
use sqlx::{Connection, PgPool};
use tokio::time::Instant;
use uuid::Uuid;

use crate::runtime::catalogs::{AccessError, RuntimeCatalogs, TableBinding};
use crate::runtime::config::{OrphanConfig, TaskSettings};
use crate::scheduling::{Scheduler, SchedulingError};

/// The execution budget of the connection a job is planned through.
///
/// The connection's own bound is the job's budget: it covers the whole job,
/// not one attempt, so every attempt and every retry must fit before the
/// deadline the plan carries.
fn execution_budget(binding: &TableBinding) -> Duration {
    Duration::from_secs(binding.connection.settings.max_execution_seconds())
}

/// The connection's execution and publication protection window in
/// milliseconds.
///
/// Files and snapshots a connection promises to keep readable for this long
/// are never aged out sooner, by expiration or by orphan cleanup.
fn protection_window_ms(binding: &TableBinding) -> i64 {
    i64::try_from(
        binding
            .connection
            .settings
            .max_execution_seconds()
            .saturating_add(
                binding
                    .connection
                    .settings
                    .max_unresolved_publication_seconds(),
            ),
    )
    .unwrap_or(i64::MAX / 1_000)
    .saturating_mul(1_000)
}

/// Caps a task's duration with the time left in the job's execution budget.
///
/// The coordinator's own limit stays authoritative: a connection may ask for a
/// shorter job, never a longer one. The remaining budget may only shorten it
/// further, so an attempt never outruns the deadline the plan carries.
fn execution_limits(
    mut limits: bergsmith_protocol::cluster::ResourceLimits,
    remaining: Option<Duration>,
) -> bergsmith_protocol::cluster::ResourceLimits {
    if let Some(remaining) = remaining {
        limits.max_duration = Some(crate::workers::capped_duration(
            limits.max_duration,
            remaining,
        ));
    }
    limits
}

/// How many jobs one pass examines.
pub const DEFAULT_PLANNING_PAGE: i64 = 32;

/// What one planning pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlanningSummary {
    /// Plans recorded.
    pub planned: u32,
    /// Jobs that had no beneficial work.
    pub no_op: u32,
    /// Tables rejected by the planner.
    pub rejected: u32,
    /// Plans whose dispatch was registered.
    pub registered: u32,
    /// Transient failures left for another pass.
    pub deferred: u32,
    /// Jobs ended because an operator must act.
    pub failed: u32,
}

/// What planning one job produced.
enum PlannedWork {
    /// A compaction plan whose tasks must be registered for dispatch.
    Rewrite(Box<PlanDocument>),
    /// A metadata-only expiration plan; the job is already `ReadyToCommit`.
    Expiration,
    /// An orphan-file plan; the job is already `ReadyToCommit`.
    OrphanCleanup,
    /// No work.
    NoOp(bergsmith_maintenance::NoOpReason),
    /// The MVP cannot maintain the table.
    Rejected,
}

/// Plans `Planning` jobs and registers dispatch for `Queued` ones.
pub struct PlannerService {
    pool: PgPool,
    catalogs: Arc<RuntimeCatalogs>,
    scheduler: Arc<Scheduler>,
    config: PlannerConfig,
    task: TaskSettings,
    admission_max_input_bytes: u64,
    retry: RetryPolicy,
    /// Whether the catalog's `remove-snapshots` rejects referenced snapshots.
    expiration_ref_safety: bool,
    /// Orphan-file cleanup limits and enablement.
    orphan: OrphanConfig,
    page: i64,
    retry_after: tokio::sync::Mutex<BTreeMap<JobId, RetryEntry>>,
    planning_cursor: std::sync::Mutex<Uuid>,
    queued_cursor: std::sync::Mutex<Uuid>,
}

#[derive(Clone, Copy)]
struct RetryEntry {
    at: Instant,
    attempt: u32,
}

impl PlannerService {
    /// Creates the loop.
    ///
    /// # Errors
    ///
    /// [`PlanError::InvalidConfig`] when the base planner limits or task
    /// envelope are inconsistent.
    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is an independent daemon dependency or safety policy"
    )]
    pub fn new(
        pool: PgPool,
        catalogs: Arc<RuntimeCatalogs>,
        scheduler: Arc<Scheduler>,
        config: PlannerConfig,
        task: TaskSettings,
        admission_max_input_bytes: u64,
        retry: RetryPolicy,
        expiration_ref_safety: bool,
        orphan: OrphanConfig,
    ) -> Result<Self, PlanError> {
        config.validate()?;
        task.limits()
            .map_err(|error| PlanError::InvalidConfig(error.to_string()))?;
        Ok(Self {
            pool,
            catalogs,
            scheduler,
            config,
            task,
            admission_max_input_bytes,
            retry,
            expiration_ref_safety,
            orphan,
            page: DEFAULT_PLANNING_PAGE,
            retry_after: tokio::sync::Mutex::new(BTreeMap::new()),
            planning_cursor: std::sync::Mutex::new(Uuid::nil()),
            queued_cursor: std::sync::Mutex::new(Uuid::nil()),
        })
    }

    /// Runs one pass immediately and then at `interval` until shutdown.
    pub async fn run(&self, interval: Duration, shutdown: impl Future<Output = ()> + Send) {
        tokio::pin!(shutdown);
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = &mut shutdown => return,
                _ = tick.tick() => {
                    tokio::select! {
                        () = &mut shutdown => return,
                        result = self.pass() => {
                            if let Err(error) = result {
                                tracing::warn!(
                                    reason_code = error.reason_code(),
                                    "planning pass failed"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Plans one bounded page of `Planning` jobs and registers one bounded
    /// page of `Queued` jobs whose dispatch is missing.
    ///
    /// # Errors
    ///
    /// A database or leadership failure that concerns the whole pass. A
    /// failure scoped to one job is logged and leaves that job for later.
    pub async fn pass(&self) -> Result<PlanningSummary, PlanningError> {
        let now = Instant::now();
        let mut summary = PlanningSummary::default();
        let cursor = *self
            .planning_cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let planning =
            jobs::in_state_after(&mut conn, JobState::Planning, cursor, self.page).await?;
        if let Some(last) = planning.last() {
            *self
                .planning_cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = *last.id.as_uuid();
        }
        if i64::try_from(planning.len()).unwrap_or(i64::MAX) < self.page {
            *self
                .planning_cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Uuid::nil();
        }
        drop(conn);
        for job in planning {
            if self.deferred(job.id, now).await {
                summary.deferred += 1;
                continue;
            }
            match self.plan_job(&job).await {
                Ok((outcome, current)) => {
                    self.clear_retry(job.id).await;
                    match outcome {
                        PlannedWork::Rewrite(plan) => {
                            summary.planned += 1;
                            match self.register_plan(&current, &plan).await {
                                Ok(()) => summary.registered += 1,
                                Err(error) if error.is_transient() => {
                                    tracing::warn!(
                                        job_id = %job.id,
                                        reason_code = error.reason_code(),
                                        "dispatch registration deferred"
                                    );
                                    summary.deferred += 1;
                                }
                                Err(error) => {
                                    tracing::error!(
                                        job_id = %job.id,
                                        reason_code = error.reason_code(),
                                        "dispatch registration failed permanently"
                                    );
                                    summary.failed +=
                                        u32::from(self.fail(&current, error.reason_code()).await?);
                                }
                            }
                        }
                        PlannedWork::Expiration | PlannedWork::OrphanCleanup => {
                            summary.planned += 1;
                        }
                        PlannedWork::NoOp(reason) => {
                            tracing::debug!(reason = reason.as_str(), "job has no work");
                            summary.no_op += 1;
                        }
                        PlannedWork::Rejected => summary.rejected += 1,
                    }
                }
                Err(PlanningError::Superseded) => {}
                Err(error) if error.is_transient() => {
                    self.defer(job.id).await;
                    summary.deferred += 1;
                    tracing::warn!(
                        job_id = %job.id,
                        reason_code = error.reason_code(),
                        "planning deferred"
                    );
                }
                Err(error) => {
                    tracing::error!(
                        job_id = %job.id,
                        reason_code = error.reason_code(),
                        "planning failed permanently"
                    );
                    summary.failed += u32::from(self.fail(&job, error.reason_code()).await?);
                }
            }
        }
        summary.registered += self.register_queued().await?;
        Ok(summary)
    }

    async fn register_queued(&self) -> Result<u32, PlanningError> {
        let cursor = *self
            .queued_cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let queued = jobs::in_state_after(&mut conn, JobState::Queued, cursor, self.page).await?;
        if let Some(last) = queued.last() {
            *self
                .queued_cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = *last.id.as_uuid();
        }
        if i64::try_from(queued.len()).unwrap_or(i64::MAX) < self.page {
            *self
                .queued_cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Uuid::nil();
        }
        drop(conn);
        let mut registered = 0;
        for job in queued {
            // Expiration plans never queue: their job is already ready to commit.
            if job.operation != JobOperation::Compact {
                continue;
            }
            let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
            if bergsmith_persistence::scheduling::dispatch_registration(&mut conn, job.id)
                .await?
                .is_some()
            {
                continue;
            }
            let Some(plan) = crate::planning::load_plan(&mut conn, job.id).await? else {
                continue;
            };
            drop(conn);
            match self.register_plan(&job, &plan).await {
                Ok(()) => registered += 1,
                Err(error) if error.is_transient() => {
                    tracing::warn!(
                        job_id = %job.id,
                        reason_code = error.reason_code(),
                        "queued dispatch registration deferred"
                    );
                }
                Err(error) => {
                    tracing::error!(
                        job_id = %job.id,
                        reason_code = error.reason_code(),
                        "queued dispatch registration failed permanently"
                    );
                    self.fail(&job, error.reason_code()).await?;
                }
            }
        }
        Ok(registered)
    }

    #[tracing::instrument(skip_all, fields(job_id = %job.id))]
    async fn plan_job(&self, job: &jobs::Job) -> Result<(PlannedWork, jobs::Job), PlanningError> {
        if job.operation == JobOperation::RemoveOrphanFiles && !self.orphan.enabled {
            return Err(PlanningError::OrphanCleanupUnavailable);
        }
        let binding = self
            .catalogs
            .binding_for_job(job)
            .await
            .map_err(PlanningError::Access)?;
        if job.operation == JobOperation::ExpireSnapshots {
            return self.plan_expiration(job, &binding).await;
        }
        if job.operation == JobOperation::RemoveOrphanFiles {
            return self.plan_orphans(job, &binding).await;
        }
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let policy = schedules::policy_for_job(&mut conn, job.id).await?;
        drop(conn);
        let config = match policy {
            Some(policy) => PlannerConfig::for_policy(
                self.config.clone(),
                &policy.specification,
                self.admission_max_input_bytes,
            )
            .map_err(PlanningError::Plan)?,
            None => self.config.clone(),
        };
        let outcome = Planner::new(config)
            .map_err(PlanningError::Plan)?
            .plan(
                PlanInput {
                    metadata: &binding.loaded.metadata,
                    metadata_location: binding.loaded.metadata_location.as_deref(),
                    file_io: binding.store.file_io(),
                    selection: &[],
                },
                PlanIds {
                    job_id: job.id,
                    plan_id: PlanId::generate(),
                },
            )
            .await
            .map_err(PlanningError::Plan)?;
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let mut tx = conn.begin().await.map_err(PersistenceError::from)?;
        let current = bergsmith_persistence::plans::lock_for_planning(
            &mut tx,
            job.id,
            job.state_revision,
            self.scheduler.epoch(),
        )
        .await
        .map_err(|error| match error {
            PersistenceError::RevisionConflict { .. } | PersistenceError::JobNotInState { .. } => {
                PlanningError::Superseded
            }
            error => PlanningError::Persistence(error),
        })?;
        let recorded = crate::planning::record_outcome(
            &mut tx,
            &current,
            &outcome,
            execution_budget(&binding),
        )
        .await
        .map_err(PlanningError::Record)?;
        bergsmith_persistence::scheduling::check_leadership(&mut tx, self.scheduler.epoch())
            .await?;
        tx.commit().await.map_err(PersistenceError::from)?;
        let planned = match outcome {
            PlanOutcome::Planned(document) => PlannedWork::Rewrite(document),
            PlanOutcome::NoOp(reason) => PlannedWork::NoOp(reason),
            PlanOutcome::Rejected(_) => PlannedWork::Rejected,
        };
        Ok((planned, recorded))
    }

    /// Plans a metadata-only snapshot expiration.
    ///
    /// The selection honors the table's properties and the connection's
    /// protection window, and excludes every snapshot the ledger pins. An
    /// empty selection ends the job `NoOp`.
    async fn plan_expiration(
        &self,
        job: &jobs::Job,
        binding: &TableBinding,
    ) -> Result<(PlannedWork, jobs::Job), PlanningError> {
        if !self.expiration_ref_safety {
            return Err(PlanningError::ExpirationRefSafetyUnavailable);
        }
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let Some(baseline) = bergsmith_maintenance::PlanningBaseline::capture(
            &binding.loaded.metadata,
            binding.loaded.metadata_location.as_deref(),
        )
        .map_err(PlanningError::Plan)?
        else {
            let reason = bergsmith_maintenance::NoOpReason::NoSnapshot;
            let recorded = self.record_noop(&mut conn, job, reason, binding).await?;
            return Ok((PlannedWork::NoOp(reason), recorded));
        };
        let protected =
            expirations::protected_snapshots(&mut conn, job.organization_id, job.table_id, job.id)
                .await?;
        let protection_ms = protection_window_ms(binding);
        let policy =
            ExpirationPolicy::from_metadata(&binding.loaded.metadata, 1, protection_ms, protected)
                .map_err(PlanningError::Expiration)?;
        let decision = plan_expiration(&binding.loaded.metadata, &policy, Utc::now())
            .map_err(PlanningError::Expiration)?;
        if decision.remove.is_empty() {
            let reason = bergsmith_maintenance::NoOpReason::NoExpiredSnapshots;
            let recorded = self.record_noop(&mut conn, job, reason, binding).await?;
            return Ok((PlannedWork::NoOp(reason), recorded));
        }
        let document =
            ExpirationPlanDocument::new(job.id, PlanId::generate(), baseline, &decision, &policy)
                .map_err(PlanningError::Plan)?;
        let content = document.to_value().map_err(PlanningError::Plan)?;
        let hash = document
            .content_hash()
            .map_err(PlanningError::Plan)?
            .to_hex();
        let recorded = expirations::create_expiration_plan(
            &mut conn,
            &NewExpirationPlan {
                id: document.plan_id,
                job_id: job.id,
                content: &content,
                content_hash: &hash,
            },
            job.state_revision,
            self.scheduler.epoch(),
        )
        .await
        .map_err(|error| match error {
            PersistenceError::RevisionConflict { .. } | PersistenceError::JobNotInState { .. } => {
                PlanningError::Superseded
            }
            error => PlanningError::Persistence(error),
        })?;
        Ok((PlannedWork::Expiration, recorded))
    }

    /// Plans an orphan-file cleanup.
    ///
    /// The listing covers the whole table location; the reachability scan
    /// reads every retained snapshot's manifests. Objects under Bergsmith's
    /// prefix, referenced objects, and objects younger than the connection's
    /// protection window or the operation's minimum age are never candidates.
    /// An empty selection ends the job `NoOp`.
    #[expect(
        clippy::too_many_lines,
        reason = "one plan keeps the gate, reachability, selection, and durable plan in one fenced step"
    )]
    async fn plan_orphans(
        &self,
        job: &jobs::Job,
        binding: &TableBinding,
    ) -> Result<(PlannedWork, jobs::Job), PlanningError> {
        if !self.orphan.enabled {
            return Err(PlanningError::OrphanCleanupUnavailable);
        }
        let Some(baseline) = bergsmith_maintenance::PlanningBaseline::capture(
            &binding.loaded.metadata,
            binding.loaded.metadata_location.as_deref(),
        )
        .map_err(PlanningError::Plan)?
        else {
            let reason = bergsmith_maintenance::NoOpReason::NoSnapshot;
            let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
            let recorded = self.record_noop(&mut conn, job, reason, binding).await?;
            return Ok((PlannedWork::NoOp(reason), recorded));
        };
        let metadata_location = binding.loaded.metadata_location.as_deref().ok_or_else(|| {
            PlanningError::Plan(PlanError::StagedTable {
                table_uuid: binding.loaded.metadata.uuid(),
            })
        })?;
        let reachable = Reachable::capture(
            &binding.loaded.metadata,
            metadata_location,
            binding.store.file_io(),
            ReachabilityBounds::new(
                self.orphan.max_scan_entries,
                self.orphan.max_scan_entries,
                self.orphan.max_scan_bytes,
            )
            .map_err(PlanningError::Orphan)?,
        )
        .await
        .map_err(PlanningError::Orphan)?;
        let table = Location::parse(binding.loaded.metadata.location())
            .map_err(OrphanError::Location)
            .map_err(PlanningError::Orphan)?;
        let listed = binding
            .store
            .list_bounded(&table, self.orphan.max_listed_objects)
            .await
            .map_err(|error| match error {
                bergsmith_storage::StorageError::ListingTooLarge { .. } => {
                    PlanningError::OrphanListingTooLarge
                }
                error => PlanningError::Access(AccessError::Storage(error)),
            })?;
        let policy = OrphanPolicy::new(
            self.orphan.minimum_age_seconds,
            protection_window_ms(binding),
            self.orphan.max_candidates,
        )
        .map_err(PlanningError::Orphan)?;
        let decision = select_orphans(
            &reachable,
            &listed,
            binding.loaded.metadata.location(),
            baseline.data_location(),
            &policy,
            Utc::now(),
        )
        .map_err(PlanningError::Orphan)?;
        if decision.candidates.is_empty() {
            let reason = bergsmith_maintenance::NoOpReason::NoOrphanFiles;
            let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
            let recorded = self.record_noop(&mut conn, job, reason, binding).await?;
            return Ok((PlannedWork::NoOp(reason), recorded));
        }
        let document = OrphanPlanDocument::new(
            job.id,
            PlanId::generate(),
            baseline,
            binding.loaded.metadata.location(),
            u64::try_from(listed.len()).unwrap_or(u64::MAX),
            &reachable,
            &decision,
        )
        .map_err(PlanningError::Plan)?;
        let content = document.to_value().map_err(PlanningError::Plan)?;
        let hash = document
            .content_hash()
            .map_err(PlanningError::Plan)?
            .to_hex();
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let recorded = orphan_plans::create_orphan_plan(
            &mut conn,
            &NewOrphanPlan {
                id: document.plan_id,
                job_id: job.id,
                content: &content,
                content_hash: &hash,
            },
            job.state_revision,
            self.scheduler.epoch(),
        )
        .await
        .map_err(|error| match error {
            PersistenceError::RevisionConflict { .. } | PersistenceError::JobNotInState { .. } => {
                PlanningError::Superseded
            }
            error => PlanningError::Persistence(error),
        })?;
        Ok((PlannedWork::OrphanCleanup, recorded))
    }

    /// Records a metadata-only no-op inside the fenced planning transaction.
    async fn record_noop(
        &self,
        conn: &mut sqlx::PgConnection,
        job: &jobs::Job,
        reason: bergsmith_maintenance::NoOpReason,
        binding: &TableBinding,
    ) -> Result<jobs::Job, PlanningError> {
        let mut tx = conn.begin().await.map_err(PersistenceError::from)?;
        let current = bergsmith_persistence::plans::lock_for_planning(
            &mut tx,
            job.id,
            job.state_revision,
            self.scheduler.epoch(),
        )
        .await
        .map_err(|error| match error {
            PersistenceError::RevisionConflict { .. } | PersistenceError::JobNotInState { .. } => {
                PlanningError::Superseded
            }
            error => PlanningError::Persistence(error),
        })?;
        let recorded = crate::planning::record_outcome(
            &mut tx,
            &current,
            &PlanOutcome::NoOp(reason),
            execution_budget(binding),
        )
        .await
        .map_err(PlanningError::Record)?;
        bergsmith_persistence::scheduling::check_leadership(&mut tx, self.scheduler.epoch())
            .await?;
        tx.commit().await.map_err(PersistenceError::from)?;
        Ok(recorded)
    }

    async fn register_plan(
        &self,
        job: &jobs::Job,
        plan: &PlanDocument,
    ) -> Result<(), RegistrationError> {
        if plan.job_id() != job.id {
            return Err(RegistrationError::Permanent("PlanJobMismatch"));
        }
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|_| RegistrationError::Transient)?;
        if bergsmith_persistence::scheduling::dispatch_registration(&mut conn, job.id)
            .await
            .map_err(|_| RegistrationError::Transient)?
            .is_some()
        {
            return Ok(());
        }
        // The remaining budget is read from the database clock, so the envelope
        // and the persisted deadline cannot disagree. The grant path reads it
        // again when an attempt starts, which is where the bound is enforced.
        let remaining = jobs::remaining_execution_budget(&mut conn, job.id)
            .await
            .map_err(|_| RegistrationError::Transient)?;
        drop(conn);
        let binding = self
            .catalogs
            .binding_for_job(job)
            .await
            .map_err(|error| RegistrationError::from_access(&error))?;
        let prefix = artifact_prefix(&binding, job.id)?;
        let limits = self
            .task
            .limits()
            .map_err(|_| RegistrationError::Permanent("TaskEnvelopeInvalid"))?;
        // The job's budget may only tighten the coordinator's: an operator keeps
        // the table's inputs readable for that long, so no attempt may outrun
        // it.
        let limits = execution_limits(limits, remaining);
        let mut offers = Vec::with_capacity(plan.tasks().len());
        for task in plan.tasks() {
            let artifact = plan
                .artifact_of(task)
                .map_err(|_| RegistrationError::Permanent("PlanArtifactInvalid"))?;
            let location = prefix
                .child(&artifact_name(task))
                .map_err(|_| RegistrationError::Permanent("PlanArtifactLocationInvalid"))?;
            // Rewriting the same immutable bytes is safe after a crash between
            // the plan row and this write.
            binding
                .store
                .write(&location, artifact.encode_to_vec().into())
                .await
                .map_err(|_| RegistrationError::Transient)?;
            offers.push(crate::workers::TaskOffer {
                organization: job.organization_id,
                task: task.task_id(),
                pool: self.task.pool.clone(),
                capability: self.task.capability.clone(),
                artifact: bergsmith_protocol::cluster::ArtifactRef {
                    uri: location.to_string(),
                    sha256: artifact_digest(&artifact).as_bytes().to_vec(),
                },
                limits,
            });
        }
        self.scheduler
            .register(job.id, &offers)
            .await
            .map_err(|error| RegistrationError::from_scheduler(&error))
    }

    async fn fail(&self, job: &jobs::Job, reason: &'static str) -> Result<bool, PlanningError> {
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        Ok(bergsmith_persistence::scheduling::stop(
            &mut conn,
            job.id,
            job.state_revision,
            self.scheduler.epoch(),
            JobState::Failed,
            reason,
        )
        .await?)
    }

    async fn defer(&self, job: JobId) {
        let mut retries = self.retry_after.lock().await;
        let attempt = retries
            .get(&job)
            .map_or(1, |entry| entry.attempt.saturating_add(1));
        let shift = attempt.saturating_sub(1).min(6);
        let milliseconds = self
            .retry
            .base_millis
            .saturating_mul(1_i64 << shift)
            .min(self.retry.max_millis)
            .max(1);
        let delay = Duration::from_millis(u64::try_from(milliseconds).unwrap_or(1));
        retries.insert(
            job,
            RetryEntry {
                at: Instant::now() + delay,
                attempt,
            },
        );
    }

    async fn clear_retry(&self, job: JobId) {
        self.retry_after.lock().await.remove(&job);
    }

    async fn deferred(&self, job: JobId, now: Instant) -> bool {
        self.retry_after
            .lock()
            .await
            .get(&job)
            .is_some_and(|entry| entry.at > now)
    }
}

fn artifact_prefix(binding: &TableBinding, job: JobId) -> Result<Location, RegistrationError> {
    binding
        .data_location
        .child("bergsmith")
        .and_then(|location| location.child(&job.to_string()))
        .and_then(|location| location.child("artifacts"))
        .map_err(|_| RegistrationError::Permanent("PlanArtifactLocationInvalid"))
}

fn artifact_name(task: &PlannedTask) -> String {
    format!("{}.pb", task.task_id())
}

/// Why planning a job could not complete.
#[derive(Debug, thiserror::Error)]
pub enum PlanningError {
    /// The ledger failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// The plan could not be built or recorded.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// Snapshot expiration could not be selected.
    #[error(transparent)]
    Expiration(#[from] ExpirationError),
    /// The catalog has no certified protection against concurrent refs.
    #[error("expiration requires catalog reference safety")]
    ExpirationRefSafetyUnavailable,
    /// Orphan-file selection failed.
    #[error(transparent)]
    Orphan(#[from] OrphanError),
    /// Orphan-file cleanup is disabled in this deployment.
    #[error("orphan-file cleanup is disabled")]
    OrphanCleanupUnavailable,
    /// The table's listing exceeds the configured bound.
    #[error("the table lists more objects than orphan cleanup may examine")]
    OrphanListingTooLarge,
    /// Recording the outcome failed.
    #[error(transparent)]
    Record(#[from] crate::planning::PlanningError),
    /// The catalog or storage could not be resolved.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// A concurrent cancellation or transition won.
    #[error("job is no longer planning at the scanned revision")]
    Superseded,
}

impl PlanningError {
    /// Whether another pass can succeed without operator action.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        match self {
            Self::Persistence(_) | Self::Orphan(OrphanError::Io(_)) => true,
            // Only metadata I/O can succeed on a retry. Invalid selections,
            // duplicate files, missing sequences, oversized inventories, and
            // malformed locations/predicates are deterministic for the same
            // metadata and must reach a terminal outcome.
            Self::Plan(error) => matches!(error, PlanError::Iceberg(_)),
            Self::Record(error) => matches!(
                error,
                crate::planning::PlanningError::Persistence(
                    PersistenceError::Database(_)
                        | PersistenceError::LeadershipLost { .. }
                        | PersistenceError::SnapshotExpiring { .. }
                )
            ),
            Self::Access(error) => error.is_transient(),
            // A bound violation, a malformed path, and a disabled operation
            // cannot succeed on retry.
            Self::Expiration(_)
            | Self::ExpirationRefSafetyUnavailable
            | Self::Orphan(_)
            | Self::OrphanCleanupUnavailable
            | Self::OrphanListingTooLarge
            | Self::Superseded => false,
        }
    }

    /// A stable reason for job state and logs.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Persistence(_) => "PlanningLedgerUnavailable",
            Self::Plan(PlanError::InvalidConfig(_)) => "PlanningConfigurationInvalid",
            Self::Plan(_) => "PlanningFailed",
            Self::Expiration(_) => "PlanningExpirationInvalid",
            Self::ExpirationRefSafetyUnavailable => "ExpirationRefSafetyUnavailable",
            Self::Orphan(_) => "PlanningOrphansInvalid",
            Self::OrphanCleanupUnavailable => "OrphanCleanupUnavailable",
            Self::OrphanListingTooLarge => "OrphanListingTooLarge",
            Self::Record(_) => "PlanningRecordFailed",
            Self::Access(error) => error.reason_code(),
            Self::Superseded => "PlanningSuperseded",
        }
    }
}

impl From<sqlx::Error> for PlanningError {
    fn from(error: sqlx::Error) -> Self {
        Self::Persistence(PersistenceError::from(error))
    }
}

/// Why dispatch registration failed.
#[derive(Debug, thiserror::Error)]
enum RegistrationError {
    /// Retrying may succeed.
    #[error("transient dispatch registration failure")]
    Transient,
    /// An operator must fix configuration.
    #[error("permanent dispatch registration failure: {0}")]
    Permanent(&'static str),
}

impl RegistrationError {
    const fn is_transient(&self) -> bool {
        matches!(self, Self::Transient)
    }

    const fn reason_code(&self) -> &'static str {
        match self {
            Self::Transient => "DispatchRegistrationUnavailable",
            Self::Permanent(reason) => reason,
        }
    }

    fn from_access(error: &AccessError) -> Self {
        if error.is_transient() {
            Self::Transient
        } else {
            Self::Permanent(error.reason_code())
        }
    }

    fn from_scheduler(error: &SchedulingError) -> Self {
        match error {
            SchedulingError::Configuration => Self::Permanent("DispatchConfigurationInvalid"),
            SchedulingError::Plan(_) => Self::Permanent("StoredPlanInvalid"),
            SchedulingError::Access(_)
            | SchedulingError::Persistence(_)
            | SchedulingError::Timeout => Self::Transient,
        }
    }
}

#[cfg(test)]
#[path = "../../../persistence/tests/support/mod.rs"]
mod support;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_remaining_budget_only_tightens_the_task_limit() {
        let limits = bergsmith_protocol::cluster::ResourceLimits {
            max_duration: Some(prost_types::Duration {
                seconds: 180,
                nanos: 0,
            }),
            ..Default::default()
        };

        // Less time left than the envelope allows.
        let capped = execution_limits(limits, Some(Duration::from_secs(60)));
        assert_eq!(
            capped.max_duration,
            Some(prost_types::Duration {
                seconds: 60,
                nanos: 0
            })
        );
        // More time left than the envelope allows never extends it.
        let capped = execution_limits(limits, Some(Duration::from_secs(3600)));
        assert_eq!(
            capped.max_duration,
            Some(prost_types::Duration {
                seconds: 180,
                nanos: 0
            })
        );
        // A job without a deadline is not bounded.
        let capped = execution_limits(limits, None);
        assert_eq!(capped.max_duration, limits.max_duration);
        // A limit the coordinator did not set is replaced by the budget.
        let unbounded = bergsmith_protocol::cluster::ResourceLimits {
            max_duration: None,
            ..Default::default()
        };
        assert_eq!(
            execution_limits(unbounded, Some(Duration::from_secs(90))).max_duration,
            Some(prost_types::Duration {
                seconds: 90,
                nanos: 0
            })
        );
    }

    #[test]
    fn a_sub_second_budget_never_becomes_a_zero_duration() {
        let limits = bergsmith_protocol::cluster::ResourceLimits {
            max_duration: Some(prost_types::Duration {
                seconds: 60,
                nanos: 0,
            }),
            ..Default::default()
        };

        // A positive budget below one second keeps its nanoseconds: a zero
        // duration is rejected by the envelope validation and by the worker.
        let capped = execution_limits(limits, Some(Duration::from_millis(950)));
        assert_eq!(
            capped.max_duration,
            Some(prost_types::Duration {
                seconds: 0,
                nanos: 950_000_000
            })
        );

        // A shorter envelope limit wins even when it is fractional.
        let short = bergsmith_protocol::cluster::ResourceLimits {
            max_duration: Some(prost_types::Duration {
                seconds: 0,
                nanos: 500_000_000,
            }),
            ..Default::default()
        };
        assert_eq!(
            execution_limits(short, Some(Duration::from_secs(5))).max_duration,
            Some(prost_types::Duration {
                seconds: 0,
                nanos: 500_000_000
            })
        );
    }

    async fn service(pool: PgPool, epoch: i64) -> PlannerService {
        use sqlx::ConnectOptions as _;
        let url = pool.connect_options().to_url_lossy().to_string();
        let database =
            bergsmith_persistence::Database::connect(&bergsmith_persistence::DatabaseConfig::new(
                bergsmith_persistence::DatabaseUrl::new(url),
            ))
            .await
            .unwrap();
        let catalogs = Arc::new(RuntimeCatalogs::new(
            database,
            crate::runtime::config::RuntimeProfiles {
                default: bergsmith_storage::StorageProfiles::default(),
                connections: BTreeMap::new(),
            },
        ));
        let access = Arc::new(crate::runtime::catalogs::RuntimeWorkerAccess::new(
            catalogs.clone(),
        ));
        let worker = crate::workers::WorkerService::new(
            pool.clone(),
            epoch,
            crate::workers::WorkerConfig::default(),
            access.clone(),
        )
        .unwrap();
        let scheduler = Arc::new(
            Scheduler::new(
                worker,
                access,
                crate::scheduling::SchedulerConfig::default(),
            )
            .unwrap(),
        );
        PlannerService::new(
            pool,
            catalogs,
            scheduler,
            PlannerConfig::default(),
            TaskSettings {
                pool: "native".into(),
                capability: "native-v1".into(),
                memory_bytes: 1024,
                cpu_millis: 1000,
                max_duration_seconds: 180,
                max_output_bytes: 1024,
            },
            1024,
            RetryPolicy::default(),
            false,
            OrphanConfig::default(),
        )
        .unwrap()
    }

    #[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
    async fn a_stale_planning_error_cannot_fail_a_queued_job(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let tenant = support::tenant(&mut conn, "acme").await;
        let id = support::job(&mut conn, &tenant).await;
        let original = jobs::job_by_id(&mut conn, id).await.unwrap();
        let epoch = support::leader(&mut conn).await;
        support::plan(&mut conn, id, 1).await;
        let queued = jobs::transition_job(
            &mut conn,
            &jobs::Transition {
                job_id: id,
                expected_revision: original.state_revision,
                to: JobState::Queued,
                reason: None,
                actor: None,
                non_publication_proven: false,
            },
        )
        .await
        .unwrap();
        let planner = service(pool.clone(), epoch).await;
        assert!(!planner.fail(&original, "CatalogUnavailable").await.unwrap());
        assert_eq!(jobs::job_by_id(&mut conn, id).await.unwrap(), queued);
    }

    #[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
    async fn shutdown_cancels_a_blocked_planning_pass(pool: PgPool) {
        let limited = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(pool.connect_options().as_ref().clone())
            .await
            .unwrap();
        let planner = service(limited.clone(), 1).await;
        let _held = limited.acquire().await.unwrap();
        let (send, receive) = tokio::sync::oneshot::channel();
        let run = planner.run(Duration::from_secs(1), async {
            receive.await.unwrap();
        });
        tokio::pin!(run);
        // Poll the loop into its first pass while its only connection is held.
        tokio::select! {
            biased;
            () = &mut run => panic!("planner ended before shutdown"),
            () = tokio::task::yield_now() => {}
        }
        send.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), run)
            .await
            .unwrap();
    }

    #[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
    async fn an_old_planner_cannot_fail_jobs_after_a_handoff(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let tenant = support::tenant(&mut conn, "acme").await;
        let id = support::job(&mut conn, &tenant).await;
        let original = jobs::job_by_id(&mut conn, id).await.unwrap();
        let first = bergsmith_persistence::leadership::acquire_leadership(
            &mut conn,
            "first",
            Duration::from_secs(60),
        )
        .await
        .unwrap()
        .unwrap();
        let planner = service(pool.clone(), first.epoch).await;
        bergsmith_persistence::leadership::release_leadership(
            &mut conn,
            &first.holder,
            first.epoch,
        )
        .await
        .unwrap();
        support::leader(&mut conn).await;
        assert!(matches!(
            planner.fail(&original, "CatalogUnavailable").await,
            Err(PlanningError::Persistence(
                PersistenceError::LeadershipLost { .. }
            ))
        ));
        assert_eq!(jobs::job_by_id(&mut conn, id).await.unwrap(), original);
    }

    #[test]
    fn deterministic_plan_errors_are_not_retried() {
        for error in [
            PlanError::InvalidConfig("bad".into()),
            PlanError::InvalidSelection("bad".into()),
            PlanError::StagedTable {
                table_uuid: Uuid::now_v7(),
            },
            PlanError::InventoryTooLarge { limit: 10 },
            PlanError::DuplicateFile {
                path: "s3://b/duplicate".into(),
            },
            PlanError::MissingSequenceNumber {
                path: "s3://b/no-sequence".into(),
            },
            PlanError::UnknownSpec { spec_id: 3 },
        ] {
            assert!(
                !PlanningError::Plan(error).is_transient(),
                "deterministic plan failures must end the job"
            );
        }
    }
}
