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
use bergsmith_common::job::JobState;
use bergsmith_maintenance::{
    PlanError, PlanIds, PlanInput, PlanOutcome, PlannedTask, Planner, PlannerConfig,
};
use bergsmith_persistence::scheduling::RetryPolicy;
use bergsmith_persistence::{PersistenceError, jobs, schedules};
use bergsmith_protocol::hashing::artifact_digest;
use bergsmith_storage::Location;
use prost::Message;
use sqlx::PgPool;
use tokio::time::Instant;
use uuid::Uuid;

use crate::runtime::catalogs::{AccessError, RuntimeCatalogs, TableBinding};
use crate::runtime::config::TaskSettings;
use crate::scheduling::{Scheduler, SchedulingError};

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

/// Plans `Planning` jobs and registers dispatch for `Queued` ones.
pub struct PlannerService {
    pool: PgPool,
    catalogs: Arc<RuntimeCatalogs>,
    scheduler: Arc<Scheduler>,
    config: PlannerConfig,
    task: TaskSettings,
    admission_max_input_bytes: u64,
    retry: RetryPolicy,
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
    pub fn new(
        pool: PgPool,
        catalogs: Arc<RuntimeCatalogs>,
        scheduler: Arc<Scheduler>,
        config: PlannerConfig,
        task: TaskSettings,
        admission_max_input_bytes: u64,
        retry: RetryPolicy,
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
                    if let Err(error) = self.pass().await {
                        tracing::warn!(
                            reason_code = error.reason_code(),
                            "planning pass failed"
                        );
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
                Ok(outcome) => {
                    self.clear_retry(job.id).await;
                    match outcome {
                        PlanOutcome::Planned(plan) => {
                            summary.planned += 1;
                            match self.register_plan(&job, &plan).await {
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
                                    self.fail(job.id, error.reason_code()).await?;
                                    summary.failed += 1;
                                }
                            }
                        }
                        PlanOutcome::NoOp(_) => summary.no_op += 1,
                        PlanOutcome::Rejected(_) => summary.rejected += 1,
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
                    self.fail(job.id, error.reason_code()).await?;
                    summary.failed += 1;
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
                    self.fail(job.id, error.reason_code()).await?;
                }
            }
        }
        Ok(registered)
    }

    async fn plan_job(&self, job: &jobs::Job) -> Result<PlanOutcome, PlanningError> {
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
        let binding = self
            .catalogs
            .binding_for_job(job)
            .await
            .map_err(PlanningError::Access)?;
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
        // The job may have been cancelled while the catalog read ran. The
        // persisted revision check rejects that without writing a plan.
        let current = jobs::job_by_id(&mut conn, job.id).await?;
        if current.state != JobState::Planning || current.state_revision != job.state_revision {
            return Err(PlanningError::Superseded);
        }
        crate::planning::record_outcome(&mut conn, &current, &outcome)
            .await
            .map_err(PlanningError::Record)?;
        Ok(outcome)
    }

    async fn register_plan(
        &self,
        job: &jobs::Job,
        plan: &bergsmith_maintenance::PlanDocument,
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

    async fn fail(&self, job: JobId, reason: &'static str) -> Result<(), PlanningError> {
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let job = jobs::job_by_id(&mut conn, job).await?;
        if job.state != JobState::Planning && job.state != JobState::Queued {
            return Ok(());
        }
        jobs::transition_job(
            &mut conn,
            &jobs::Transition {
                job_id: job.id,
                expected_revision: job.state_revision,
                to: JobState::Failed,
                reason: Some(reason.to_owned()),
                actor: None,
                non_publication_proven: false,
            },
        )
        .await?;
        Ok(())
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
            Self::Persistence(_) => true,
            // Only metadata I/O can succeed on a retry. Invalid selections,
            // duplicate files, missing sequences, oversized inventories, and
            // malformed locations/predicates are deterministic for the same
            // metadata and must reach a terminal outcome.
            Self::Plan(error) => matches!(error, PlanError::Iceberg(_)),
            Self::Record(error) => matches!(
                error,
                crate::planning::PlanningError::Persistence(
                    PersistenceError::Database(_) | PersistenceError::LeadershipLost { .. }
                )
            ),
            Self::Access(error) => error.is_transient(),
            Self::Superseded => false,
        }
    }

    /// A stable reason for job state and logs.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Persistence(_) => "PlanningLedgerUnavailable",
            Self::Plan(PlanError::InvalidConfig(_)) => "PlanningConfigurationInvalid",
            Self::Plan(_) => "PlanningFailed",
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
mod tests {
    use super::*;

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
