//! The publication loop (BS-020.T1).
//!
//! A job reaches `ReadyToCommit` when every planned task has an accepted
//! receipt. This loop reads that job's immutable plan, rechecks the planning
//! baseline, prepares the durable intent, and sends the first authorized
//! commit. It never resends: an authorized but unresolved send is left to the
//! scheduler's evidence-based recovery.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bergsmith_catalog::{Baseline, CatalogError};
use bergsmith_common::ids::JobId;
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_maintenance::PlanningBaseline;
use bergsmith_maintenance::orphans::{
    CandidateVerdict, OrphanError, OrphanPlanDocument, OrphanPolicy, ReachabilityBounds, Reachable,
    verify_candidate,
};
use bergsmith_persistence::orphans::{self as orphan_plans, Disposition, Receipt};
use bergsmith_persistence::scheduling::RetryPolicy;
use bergsmith_persistence::{PersistenceError, jobs, publications};
use bergsmith_storage::Location;
use chrono::Utc;
use sqlx::PgPool;
use tokio::time::Instant;
use uuid::Uuid;

use crate::durable_publication::{self, DurablePublicationError, PublicationConfig};
use crate::runtime::catalogs::{AccessError, RuntimeCatalogs};
use crate::runtime::config::OrphanConfig;
use crate::runtime::planning::DEFAULT_PLANNING_PAGE;
use crate::runtime::statistics::StatisticsCollector;

/// What one publication pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublicationSummary {
    /// Commits confirmed.
    pub published: u32,
    /// Jobs that ended without publishing because the baseline changed.
    pub replanned: u32,
    /// Jobs that ended without publishing for another reason.
    pub failed: u32,
    /// Transient failures left for another pass.
    pub deferred: u32,
    /// Publication outcomes that remain unknown.
    pub unresolved: u32,
    /// Orphan-deletion passes that ran.
    pub cleaned: u32,
}

impl PublicationSummary {
    const fn record(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Published => self.published += 1,
            Outcome::Replanned => self.replanned += 1,
            Outcome::Failed => self.failed += 1,
            Outcome::Unresolved => self.unresolved += 1,
            Outcome::Superseded => {}
            Outcome::Cleaned => self.cleaned += 1,
        }
    }
}

/// Prepares and publishes `ReadyToCommit` jobs.
pub struct PublicationService {
    pool: PgPool,
    catalogs: Arc<RuntimeCatalogs>,
    epoch: i64,
    config: PublicationConfig,
    retry: RetryPolicy,
    page: i64,
    retry_after: tokio::sync::Mutex<BTreeMap<JobId, RetryEntry>>,
    cursor: std::sync::Mutex<Uuid>,
    intent_cursor: std::sync::Mutex<Uuid>,
    deleting_cursor: std::sync::Mutex<Uuid>,
    statistics: Option<Arc<StatisticsCollector>>,
    /// Whether the catalog's `remove-snapshots` rejects referenced snapshots.
    expiration_ref_safety: bool,
    /// Orphan-file cleanup limits and enablement.
    orphan: OrphanConfig,
}

#[derive(Clone, Copy)]
struct RetryEntry {
    at: Instant,
    attempt: u32,
}

impl PublicationService {
    /// Creates the loop.
    #[must_use]
    pub fn new(
        pool: PgPool,
        catalogs: Arc<RuntimeCatalogs>,
        epoch: i64,
        config: PublicationConfig,
        retry: RetryPolicy,
        expiration_ref_safety: bool,
        orphan: OrphanConfig,
    ) -> Self {
        Self {
            pool,
            catalogs,
            epoch,
            config,
            retry,
            page: DEFAULT_PLANNING_PAGE,
            retry_after: tokio::sync::Mutex::new(BTreeMap::new()),
            cursor: std::sync::Mutex::new(Uuid::nil()),
            intent_cursor: std::sync::Mutex::new(Uuid::nil()),
            deleting_cursor: std::sync::Mutex::new(Uuid::nil()),
            statistics: None,
            expiration_ref_safety,
            orphan,
        }
    }

    /// Samples a table right after it publishes, in addition to the interval.
    #[must_use]
    pub fn with_statistics(mut self, statistics: Arc<StatisticsCollector>) -> Self {
        self.statistics = Some(statistics);
        self
    }

    /// Asks for a post-publication sample without blocking the pass.
    fn refresh_statistics(&self, job: &jobs::Job) {
        if let Some(statistics) = &self.statistics {
            statistics.request_refresh(job);
        }
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
                            "publication pass failed"
                        );
                    }
                }
            }
        }
    }

    /// Prepares and publishes one bounded page.
    ///
    /// # Errors
    ///
    /// A database failure that concerns the whole pass. A failure scoped to
    /// one job is logged and leaves that job's durable state unchanged.
    pub async fn pass(&self) -> Result<PublicationSummary, PublicationError> {
        let now = Instant::now();
        let mut summary = PublicationSummary::default();
        let ready = self
            .next_in_state(JobState::ReadyToCommit, &self.cursor)
            .await?;
        let mut processed = std::collections::BTreeSet::new();
        self.process_jobs(ready, now, &mut summary, &mut processed)
            .await?;
        // A deletion pass that did not finish leaves the job `Deleting`; the
        // next pass resumes it from the durable receipts.
        let deleting = self
            .next_in_state(JobState::Deleting, &self.deleting_cursor)
            .await?;
        self.process_jobs(deleting, now, &mut summary, &mut processed)
            .await?;
        // A crash after intent creation, or a catalog failure before send
        // authorization, leaves Committing with no publication_started_at.
        // Evidence recovery cannot resolve that state, so resume the first
        // send through the fenced publish path.
        let cursor = *self
            .intent_cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let intents = publications::unauthorized_sends_after(&mut conn, cursor, self.page).await?;
        if let Some(last) = intents.last() {
            *self
                .intent_cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = *last.id.as_uuid();
        }
        if i64::try_from(intents.len()).unwrap_or(i64::MAX) < self.page {
            *self
                .intent_cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Uuid::nil();
        }
        drop(conn);
        for job in intents {
            if self.deferred(job.id, now).await {
                summary.deferred += 1;
                continue;
            }
            match self.resume_intent(&job).await {
                Ok(outcome) => {
                    self.clear_retry(job.id).await;
                    if outcome == Outcome::Published {
                        self.refresh_statistics(&job);
                    }
                    summary.record(outcome);
                }
                Err(error) if error.is_transient() => {
                    self.defer(job.id).await;
                    summary.deferred += 1;
                    tracing::warn!(
                        job_id = %job.id,
                        reason_code = error.reason_code(),
                        "publication resume deferred"
                    );
                }
                Err(error) => {
                    tracing::error!(
                        job_id = %job.id,
                        reason_code = error.reason_code(),
                        "publication resume failed permanently"
                    );
                    if job.operation == JobOperation::RemoveOrphanFiles {
                        self.terminate_orphan(&job, error.terminal_state(), error.reason_code())
                            .await?;
                    } else {
                        self.stop(&job, error.terminal_state(), error.reason_code())
                            .await?;
                    }
                    summary.failed += 1;
                }
            }
        }
        Ok(summary)
    }

    /// Reads one page of `state`, advancing the cursor for the next pass.
    async fn next_in_state(
        &self,
        state: JobState,
        cursor: &std::sync::Mutex<Uuid>,
    ) -> Result<Vec<jobs::Job>, PublicationError> {
        let after = *cursor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let page = jobs::in_state_after(&mut conn, state, after, self.page).await?;
        if let Some(last) = page.last() {
            *cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = *last.id.as_uuid();
        }
        if i64::try_from(page.len()).unwrap_or(i64::MAX) < self.page {
            *cursor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Uuid::nil();
        }
        Ok(page)
    }

    /// Runs one publication or deletion step per job and records the outcome.
    async fn process_jobs(
        &self,
        jobs: Vec<jobs::Job>,
        now: Instant,
        summary: &mut PublicationSummary,
        processed: &mut std::collections::BTreeSet<JobId>,
    ) -> Result<(), PublicationError> {
        for job in jobs {
            // A deletion that started this pass is not resumed again by the
            // same pass; its next bounded batch belongs to the next tick.
            if !processed.insert(job.id) {
                continue;
            }
            if self.deferred(job.id, now).await {
                summary.deferred += 1;
                continue;
            }
            match self.publish_job(&job).await {
                Ok(outcome) => {
                    self.clear_retry(job.id).await;
                    summary.record(outcome);
                    if outcome == Outcome::Published {
                        self.refresh_statistics(&job);
                    }
                }
                Err(error) if error.is_transient() => {
                    self.defer(job.id).await;
                    summary.deferred += 1;
                    tracing::warn!(
                        job_id = %job.id,
                        reason_code = error.reason_code(),
                        "publication deferred"
                    );
                }
                Err(error) => {
                    tracing::error!(
                        job_id = %job.id,
                        reason_code = error.reason_code(),
                        "publication failed permanently"
                    );
                    if job.operation == JobOperation::RemoveOrphanFiles {
                        self.terminate_orphan(&job, error.terminal_state(), error.reason_code())
                            .await?;
                    } else {
                        self.stop(&job, error.terminal_state(), error.reason_code())
                            .await?;
                    }
                    summary.failed += 1;
                }
            }
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(job_id = %job.id))]
    async fn resume_intent(&self, job: &jobs::Job) -> Result<Outcome, PublicationError> {
        if job.operation == JobOperation::ExpireSnapshots && !self.expiration_ref_safety {
            // No send was authorized: the intent is durable but unsent, so a
            // restart with the guarantee disabled can fail it safely.
            self.stop(job, JobState::Failed, "ExpirationRefSafetyUnavailable")
                .await?;
            return Ok(Outcome::Failed);
        }
        let connection = {
            let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
            self.catalogs
                .intent_connection(&mut conn, job)
                .await
                .map_err(PublicationError::Access)?
        };
        let catalog = self
            .catalogs
            .catalog_for(&connection)
            .await
            .map_err(PublicationError::Access)?;
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let published = if job.operation == JobOperation::ExpireSnapshots {
            durable_publication::publish_expiration(&mut conn, job, self.epoch, &catalog).await
        } else {
            durable_publication::publish(&mut conn, job, self.epoch, &catalog).await
        };
        match published {
            Ok(job) => Ok(match job.state {
                JobState::Succeeded => Outcome::Published,
                JobState::CommitOutcomeUnknown => Outcome::Unresolved,
                JobState::ReplanRequired => Outcome::Replanned,
                JobState::Failed | JobState::Cancelled => Outcome::Failed,
                _ => Outcome::Superseded,
            }),
            Err(DurablePublicationError::Persistence(
                PersistenceError::JobNotInState { .. }
                | PersistenceError::RevisionConflict { .. }
                | PersistenceError::PublicationAlreadyAuthorized { .. },
            )) => Ok(Outcome::Superseded),
            Err(DurablePublicationError::Persistence(
                PersistenceError::MaintenancePolicyDisabled { .. },
            )) => {
                drop(conn);
                self.stop(job, JobState::Failed, "PolicyDisabled").await?;
                Ok(Outcome::Failed)
            }
            Err(error) => Err(PublicationError::Durable(error)),
        }
    }

    #[tracing::instrument(skip_all, fields(job_id = %job.id))]
    async fn publish_job(&self, job: &jobs::Job) -> Result<Outcome, PublicationError> {
        if job.operation == JobOperation::ExpireSnapshots {
            if !self.expiration_ref_safety {
                self.stop(job, JobState::Failed, "ExpirationRefSafetyUnavailable")
                    .await?;
                return Ok(Outcome::Failed);
            }
            return self.publish_expiration_job(job).await;
        }
        if job.operation == JobOperation::RemoveOrphanFiles {
            return self.publish_orphan_job(job).await;
        }
        let binding = self
            .catalogs
            .binding_for_job(job)
            .await
            .map_err(PublicationError::Access)?;
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let plan = crate::planning::load_plan(&mut conn, job.id)
            .await
            .map_err(PublicationError::Planning)?
            .ok_or(PublicationError::PlanMissing)?;
        let captured = PlanningBaseline::capture(
            &binding.loaded.metadata,
            binding.loaded.metadata_location.as_deref(),
        )?;
        if captured.as_ref() != Some(plan.baseline()) {
            drop(conn);
            self.stop(job, JobState::ReplanRequired, "PublicationBaselineChanged")
                .await?;
            return Ok(Outcome::Replanned);
        }
        let baseline = Baseline::new(&binding.loaded, binding.store.file_io().clone())
            .map_err(|_| PublicationError::Proposal)?;
        let prepared = durable_publication::prepare_publication_with(
            &mut conn,
            job,
            self.epoch,
            binding.connection.id,
            &binding.catalog,
            &baseline,
            &binding.store,
            self.config,
        )
        .await;
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(DurablePublicationError::BaselineChanged(_)) => {
                drop(conn);
                self.stop(job, JobState::ReplanRequired, "PublicationBaselineChanged")
                    .await?;
                return Ok(Outcome::Replanned);
            }
            Err(DurablePublicationError::Persistence(
                PersistenceError::JobNotInState { .. } | PersistenceError::RevisionConflict { .. },
            )) => return Ok(Outcome::Superseded),
            Err(DurablePublicationError::Persistence(
                PersistenceError::MaintenancePolicyDisabled { .. },
            )) => {
                drop(conn);
                self.stop(job, JobState::Failed, "PolicyDisabled").await?;
                return Ok(Outcome::Failed);
            }
            Err(error) => return Err(PublicationError::Durable(error)),
        };
        if prepared.state != JobState::Committing {
            return Ok(match prepared.state {
                JobState::Succeeded => Outcome::Published,
                JobState::Failed => Outcome::Failed,
                JobState::ReplanRequired => Outcome::Replanned,
                JobState::CommitOutcomeUnknown => Outcome::Unresolved,
                _ => Outcome::Superseded,
            });
        }
        match durable_publication::publish(&mut conn, &prepared, self.epoch, &binding.catalog).await
        {
            Ok(job) => Ok(match job.state {
                JobState::Succeeded => Outcome::Published,
                JobState::CommitOutcomeUnknown => Outcome::Unresolved,
                JobState::ReplanRequired => Outcome::Replanned,
                JobState::Failed | JobState::Cancelled => Outcome::Failed,
                _ => Outcome::Superseded,
            }),
            Err(DurablePublicationError::Persistence(
                PersistenceError::JobNotInState { .. } | PersistenceError::RevisionConflict { .. },
            )) => Ok(Outcome::Superseded),
            Err(error) => {
                // The intent and any authorization are durable. The scheduler
                // recovers from retained evidence; never resend here.
                tracing::warn!(
                    job_id = %job.id,
                    reason_code = "PublicationOutcomeUnknown",
                    "publication left for scheduler recovery"
                );
                match error {
                    DurablePublicationError::Catalog(_) => Ok(Outcome::Unresolved),
                    _ => Err(PublicationError::Durable(error)),
                }
            }
        }
    }

    /// Prepares and sends one metadata-only expiration commit.
    async fn publish_expiration_job(&self, job: &jobs::Job) -> Result<Outcome, PublicationError> {
        let binding = self
            .catalogs
            .binding_for_job(job)
            .await
            .map_err(PublicationError::Access)?;
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let plan = durable_publication::load_expiration_plan(&mut conn, job.id)
            .await
            .map_err(PublicationError::Durable)?;
        let captured = PlanningBaseline::capture(
            &binding.loaded.metadata,
            binding.loaded.metadata_location.as_deref(),
        )?;
        if captured.as_ref() != Some(&plan.baseline) {
            drop(conn);
            self.stop(job, JobState::ReplanRequired, "PublicationBaselineChanged")
                .await?;
            return Ok(Outcome::Replanned);
        }
        let baseline = Baseline::new(&binding.loaded, binding.store.file_io().clone())
            .map_err(|_| PublicationError::Proposal)?;
        let prepared = durable_publication::prepare_expiration(
            &mut conn,
            job,
            self.epoch,
            binding.connection.id,
            &baseline,
            &plan,
        )
        .await;
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(DurablePublicationError::ProtectedSnapshot(_)) => {
                drop(conn);
                self.stop(job, JobState::ReplanRequired, "ExpirationProtectedSnapshot")
                    .await?;
                return Ok(Outcome::Replanned);
            }
            Err(DurablePublicationError::Persistence(
                PersistenceError::JobNotInState { .. } | PersistenceError::RevisionConflict { .. },
            )) => return Ok(Outcome::Superseded),
            Err(error) => return Err(PublicationError::Durable(error)),
        };
        if prepared.state != JobState::Committing {
            return Ok(match prepared.state {
                JobState::Succeeded => Outcome::Published,
                JobState::Failed => Outcome::Failed,
                JobState::ReplanRequired => Outcome::Replanned,
                JobState::CommitOutcomeUnknown => Outcome::Unresolved,
                _ => Outcome::Superseded,
            });
        }
        match durable_publication::publish_expiration(
            &mut conn,
            &prepared,
            self.epoch,
            &binding.catalog,
        )
        .await
        {
            Ok(job) => Ok(match job.state {
                JobState::Succeeded => Outcome::Published,
                JobState::CommitOutcomeUnknown => Outcome::Unresolved,
                JobState::ReplanRequired => Outcome::Replanned,
                JobState::Failed | JobState::Cancelled => Outcome::Failed,
                _ => Outcome::Superseded,
            }),
            Err(DurablePublicationError::Persistence(
                PersistenceError::JobNotInState { .. } | PersistenceError::RevisionConflict { .. },
            )) => Ok(Outcome::Superseded),
            Err(error) => {
                tracing::warn!(
                    job_id = %job.id,
                    reason_code = "ExpirationOutcomeUnknown",
                    "expiration publication left for recovery"
                );
                match error {
                    DurablePublicationError::Catalog(_) => Ok(Outcome::Unresolved),
                    _ => Err(PublicationError::Durable(error)),
                }
            }
        }
    }

    /// Executes one bounded orphan-deletion batch (ADR 0062).
    ///
    /// The first pass moves the job from `ReadyToCommit` to `Deleting`; the
    /// batch itself loads the catalog, proves what the table and every
    /// unresolved publication reference, and lists the table before it fences
    /// the destructive step on the leader lease. Connections are acquired per
    /// step, so one pool connection is enough.
    async fn publish_orphan_job(&self, job: &jobs::Job) -> Result<Outcome, PublicationError> {
        if !self.orphan.enabled {
            self.terminate_orphan(job, JobState::Failed, "OrphanCleanupUnavailable")
                .await?;
            return Ok(Outcome::Failed);
        }
        let document = {
            let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
            let (content, stored_hash) = orphan_plans::orphan_plan(&mut conn, job.id)
                .await?
                .ok_or(PublicationError::PlanMissing)?;
            let document = OrphanPlanDocument::from_persisted(content)?;
            if document.job_id != job.id || document.content_hash()?.to_hex() != stored_hash {
                return Err(PublicationError::PlanMissing);
            }
            if job.state == JobState::ReadyToCommit {
                match orphan_plans::begin_deletion(
                    &mut conn,
                    job.id,
                    job.state_revision,
                    self.epoch,
                )
                .await
                {
                    Ok(_) => {}
                    Err(
                        PersistenceError::RevisionConflict { .. }
                        | PersistenceError::JobNotInState { .. },
                    ) => return Ok(Outcome::Superseded),
                    Err(error) => return Err(error.into()),
                }
            }
            document
        };
        self.delete_orphan_batch(job, &document).await
    }

    /// Deletes one bounded batch and records a receipt per resolved candidate.
    ///
    /// Everything that only reads the table (catalog load, reachability,
    /// listing) happens before the batch is authorized, so the destructive
    /// step is fenced as late as possible. No pool connection is held across
    /// those reads or across the deletes.
    #[expect(
        clippy::too_many_lines,
        reason = "one batch keeps re-verification, receipts, and completion in one durable step"
    )]
    async fn delete_orphan_batch(
        &self,
        job: &jobs::Job,
        document: &OrphanPlanDocument,
    ) -> Result<Outcome, PublicationError> {
        let binding = self
            .catalogs
            .binding_for_job(job)
            .await
            .map_err(PublicationError::Access)?;
        let metadata_location = binding
            .loaded
            .metadata_location
            .as_deref()
            .ok_or(PublicationError::Proposal)?;
        // Manifests of a publication that may still be sent are not reachable
        // from any snapshot, so they must be protected explicitly.
        let unresolved = {
            let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
            orphan_plans::unresolved_manifest_lists(
                &mut conn,
                job.organization_id,
                job.table_id,
                job.id,
            )
            .await?
        };
        let reachable = Reachable::capture(
            &binding.loaded.metadata,
            metadata_location,
            &unresolved,
            binding.store.file_io(),
            ReachabilityBounds::new(
                self.orphan.max_scan_entries,
                self.orphan.max_scan_entries,
                self.orphan.max_scan_bytes,
            )?,
        )
        .await?;
        let table =
            Location::parse(binding.loaded.metadata.location()).map_err(OrphanError::Location)?;
        let prefixes = bergsmith_maintenance::orphans::protected_prefixes(
            binding.loaded.metadata.location(),
            &document.data_location,
        )?;
        // A fresh listing supplies the current modification time and size: the
        // plan's values are from planning and may name an object that was
        // replaced since.
        let listed = binding
            .store
            .list_bounded(&table, self.orphan.max_listed_objects)
            .await
            .map_err(|error| match error {
                bergsmith_storage::StorageError::ListingTooLarge { limit } => {
                    PublicationError::Orphans(OrphanError::ListingTooLarge { limit })
                }
                error => PublicationError::Orphans(OrphanError::Storage(error)),
            })?;
        let current: BTreeMap<Location, (u64, Option<i64>)> = listed
            .into_iter()
            .map(|entry| (entry.location, (entry.size, entry.last_modified_ms)))
            .collect();
        let protection_ms = i64::try_from(
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
        .saturating_mul(1_000);
        let policy = OrphanPolicy::new(
            self.orphan.minimum_age_seconds,
            protection_ms,
            self.orphan.max_candidates,
        )?;
        let cutoff_ms = policy.cutoff_ms(Utc::now());
        // Fence the irreversible step as late as possible: this proves the
        // lease, the `Deleting` state, and the absence of cancellation, and
        // returns the revision the batch completes at.
        let (job, recorded) = {
            let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
            let job = match orphan_plans::authorize_batch(&mut conn, job.id, self.epoch).await {
                Ok(job) => job,
                Err(
                    PersistenceError::RevisionConflict { .. }
                    | PersistenceError::JobNotInState { .. },
                ) => return Ok(Outcome::Superseded),
                Err(error) => return Err(error.into()),
            };
            let recorded = orphan_plans::recorded_paths(&mut conn, job.id).await?;
            (job, recorded)
        };
        let mut receipts = Vec::new();
        let mut resolved = 0usize;
        for candidate in &document.candidates {
            if resolved >= self.orphan.max_deletions_per_pass {
                break;
            }
            if recorded.contains(&candidate.path) {
                continue;
            }
            let location = Location::parse(&candidate.path).map_err(OrphanError::Location)?;
            let fresh = current.get(&location);
            let disposition = match verify_candidate(
                candidate, &location, fresh, &reachable, &prefixes, &table, cutoff_ms,
            ) {
                CandidateVerdict::Delete => {
                    binding
                        .store
                        .delete(&location)
                        .await
                        .map_err(OrphanError::Storage)?;
                    Disposition::Deleted
                }
                CandidateVerdict::Skip => Disposition::Skipped,
                CandidateVerdict::Absent => Disposition::Absent,
            };
            receipts.push(Receipt {
                path: &candidate.path,
                disposition,
                size_bytes: candidate.size_bytes,
            });
            resolved = resolved.saturating_add(1);
        }
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        orphan_plans::record_receipts(&mut conn, job.id, &receipts, self.epoch).await?;
        let (count, _bytes) = orphan_plans::receipt_totals(&mut conn, job.id).await?;
        if u64::try_from(count).unwrap_or(u64::MAX)
            >= u64::try_from(document.candidates.len()).unwrap_or(u64::MAX)
        {
            self.end_orphan(&mut conn, &job, JobState::Succeeded, "OrphanFilesRemoved")
                .await?;
        }
        Ok(Outcome::Cleaned)
    }

    /// Ends a `Deleting` job in `to` with its current revision.
    async fn end_orphan(
        &self,
        conn: &mut sqlx::PgConnection,
        job: &jobs::Job,
        to: JobState,
        reason: &'static str,
    ) -> Result<(), PersistenceError> {
        match orphan_plans::end_deletion(conn, job.id, job.state_revision, self.epoch, to, reason)
            .await
        {
            Ok(_)
            | Err(
                PersistenceError::RevisionConflict { .. } | PersistenceError::JobNotInState { .. },
            ) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Ends an orphan job that will not continue, using its current revision.
    async fn terminate_orphan(
        &self,
        job: &jobs::Job,
        to: JobState,
        reason: &'static str,
    ) -> Result<(), PublicationError> {
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        let current = jobs::job(&mut conn, job.organization_id, job.id).await?;
        if !matches!(current.state, JobState::ReadyToCommit | JobState::Deleting) {
            return Ok(());
        }
        // A cleanup can never replan its way forward: any permanent failure
        // ends it. `Deleting` has no `ReplanRequired` edge for that reason.
        let to = if to == JobState::ReplanRequired {
            JobState::Failed
        } else {
            to
        };
        match orphan_plans::end_deletion(
            &mut conn,
            current.id,
            current.state_revision,
            self.epoch,
            to,
            reason,
        )
        .await
        {
            Ok(_)
            | Err(
                PersistenceError::RevisionConflict { .. } | PersistenceError::JobNotInState { .. },
            ) => Ok(()),
            Err(error) => Err(PublicationError::Persistence(error)),
        }
    }

    async fn stop(
        &self,
        job: &jobs::Job,
        to: JobState,
        reason: &'static str,
    ) -> Result<(), PersistenceError> {
        let mut conn = self.pool.acquire().await.map_err(PersistenceError::from)?;
        publications::resolve_unsent(
            &mut conn,
            job.id,
            job.state_revision,
            self.epoch,
            to,
            reason,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Published,
    Replanned,
    Failed,
    Unresolved,
    /// An orphan-deletion pass ran, whether or not every candidate is done.
    Cleaned,
    Superseded,
}

/// Why a publication pass could not complete.
#[derive(Debug, thiserror::Error)]
pub enum PublicationError {
    /// The ledger failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// The immutable plan could not be read or verified.
    #[error(transparent)]
    Planning(#[from] crate::planning::PlanningError),
    /// The current metadata could not be captured as a baseline.
    #[error(transparent)]
    Plan(#[from] bergsmith_maintenance::PlanError),
    /// The catalog or storage could not be resolved.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// Publication preparation or the send failed.
    #[error(transparent)]
    Durable(#[from] DurablePublicationError),
    /// A `ReadyToCommit` job has no plan.
    #[error("ready job has no plan")]
    PlanMissing,
    /// The baseline could not be pinned for a commit.
    #[error("publication proposal could not be built")]
    Proposal,
    /// Orphan-file selection or deletion failed.
    #[error(transparent)]
    Orphans(#[from] OrphanError),
}

impl PublicationError {
    /// Whether another pass can succeed without operator action.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Persistence(_) => true,
            // A corrupt manifest or an unsupported feature cannot improve on
            // retry; a storage read can.
            Self::Orphans(OrphanError::Io(error)) => !matches!(
                error.kind(),
                iceberg::ErrorKind::DataInvalid | iceberg::ErrorKind::FeatureUnsupported
            ),
            Self::Orphans(OrphanError::Storage(error)) => {
                matches!(error, bergsmith_storage::StorageError::Io(_))
            }
            Self::Access(error) => error.is_transient(),
            Self::Durable(error) => !matches!(
                error,
                DurablePublicationError::Catalog(CatalogError::Status {
                    status: 403 | 404,
                    ..
                }) | DurablePublicationError::BaselineChanged(_)
                    | DurablePublicationError::InvalidIntent(_)
            ),
            Self::PlanMissing
            | Self::Proposal
            | Self::Orphans(_)
            | Self::Planning(_)
            | Self::Plan(_) => false,
        }
    }

    /// The terminal state a permanent failure should record.
    #[must_use]
    pub const fn terminal_state(&self) -> JobState {
        match self {
            Self::Durable(
                DurablePublicationError::BaselineChanged(_)
                | DurablePublicationError::Catalog(CatalogError::Status { status: 404, .. }),
            ) => JobState::ReplanRequired,
            _ => JobState::Failed,
        }
    }

    /// A stable reason for logs and job state.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Persistence(_) => "PublicationLedgerUnavailable",
            Self::Planning(_) | Self::Plan(_) => "PublicationPlanInvalid",
            Self::Access(error) => error.reason_code(),
            Self::Durable(DurablePublicationError::BaselineChanged(_)) => {
                "PublicationBaselineChanged"
            }
            Self::Durable(DurablePublicationError::Catalog(CatalogError::Status {
                status: 404,
                ..
            })) => "PlanningTableMissing",
            Self::Durable(DurablePublicationError::Catalog(
                CatalogError::Status { status: 403, .. } | CatalogError::Authentication { .. },
            )) => "ExecutionAuthorityRevoked",
            Self::Durable(_) => "PublicationFailed",
            Self::PlanMissing => "PlanMissing",
            Self::Orphans(_) => "OrphanCleanupInvalid",
            Self::Proposal => "PublicationProposalInvalid",
        }
    }
}

impl From<sqlx::Error> for PublicationError {
    fn from(error: sqlx::Error) -> Self {
        Self::Persistence(PersistenceError::from(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_authentication_failures_remain_retryable() {
        for status in [401, 419] {
            let error =
                PublicationError::Durable(DurablePublicationError::Catalog(CatalogError::Status {
                    operation: "GET load table",
                    status,
                    model: None,
                }));
            assert!(error.is_transient(), "status {status}");
        }
        let error = PublicationError::Durable(DurablePublicationError::Catalog(
            CatalogError::Authentication { status: 401 },
        ));
        assert!(error.is_transient());
    }
}
