//! Recording a job's planning outcome (BS-009.T6).
//!
//! The planner in `bergsmith-maintenance` builds the plan from metadata;
//! this module makes it durable. A plan and the job's move to `Queued`
//! commit in one transaction, so a job never sits in `Planning` with a plan
//! or in `Queued` without one.

use bergsmith_common::ids::JobId;
use bergsmith_common::job::JobState;
use bergsmith_maintenance::{PlanDocument, PlanError, PlanOutcome};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::jobs::{self, Job, Transition};
use bergsmith_persistence::plans::{self, NewPlan};
use sqlx::{Connection, PgConnection};

/// The job state reason for a table the MVP cannot maintain.
pub const UNSUPPORTED_TABLE_REASON: &str = "unsupported_table";

/// Why a planning outcome could not be recorded.
#[derive(Debug, thiserror::Error)]
pub enum PlanningError {
    /// The plan was built for a different job.
    #[error("plan for job {plan_job} cannot be recorded for job {job}")]
    JobMismatch {
        /// The job being planned.
        job: JobId,
        /// The job the plan names.
        plan_job: JobId,
    },
    /// The plan could not be encoded or a stored plan failed verification.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// Writing failed; nothing was recorded.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

/// Records `outcome` for `job`, which must be `Planning` at the revision
/// the caller read.
///
/// - A plan is persisted with its tasks, all `Pending`, and the job moves
///   to `Queued`, in one transaction.
/// - The job's execution deadline is written with the plan: the whole job,
///   every attempt and every retry, must fit in `execution_budget`, counted
///   from the moment the plan is persisted.
/// - No work moves the job to `NoOp`, with the reason as its state reason.
/// - A rejected table moves the job to `Failed` with
///   [`UNSUPPORTED_TABLE_REASON`].
///
/// Returns the job after the transition.
///
/// # Errors
///
/// - [`PlanningError::JobMismatch`] when the plan names another job.
/// - [`PlanningError::Persistence`] with
///   [`PersistenceError::RevisionConflict`] or
///   [`PersistenceError::JobNotInState`] when the job changed, for example
///   because it was cancelled, or [`PersistenceError::AlreadyExists`] when it
///   already has a plan. Nothing is written in any of these cases.
pub async fn record_outcome(
    conn: &mut PgConnection,
    job: &Job,
    outcome: &PlanOutcome,
    execution_budget: std::time::Duration,
) -> Result<Job, PlanningError> {
    let transition = |to: JobState, reason: Option<&str>| Transition {
        job_id: job.id,
        expected_revision: job.state_revision,
        to,
        reason: reason.map(str::to_owned),
        actor: None,
        non_publication_proven: false,
    };
    match outcome {
        PlanOutcome::Planned(document) => {
            if document.job_id() != job.id {
                return Err(PlanningError::JobMismatch {
                    job: job.id,
                    plan_job: document.job_id(),
                });
            }
            let new = NewPlan {
                id: document.plan_id(),
                job_id: job.id,
                content: document.to_value()?,
                content_hash: document.sha256()?.to_hex(),
                task_ids: document.task_ids(),
                execution_budget,
            };
            let mut tx = conn.begin().await.map_err(PersistenceError::from)?;
            plans::create_plan(&mut tx, &new).await?;
            let queued = jobs::transition_job(&mut tx, &transition(JobState::Queued, None)).await?;
            tx.commit().await.map_err(PersistenceError::from)?;
            Ok(queued)
        }
        PlanOutcome::NoOp(reason) => Ok(jobs::transition_job(
            conn,
            &transition(JobState::NoOp, Some(reason.as_str())),
        )
        .await?),
        PlanOutcome::Rejected(_) => Ok(jobs::transition_job(
            conn,
            &transition(JobState::Failed, Some(UNSUPPORTED_TABLE_REASON)),
        )
        .await?),
    }
}

/// Reads a job's plan and verifies it against its stored hash.
///
/// # Errors
///
/// [`PlanningError::Plan`] with [`PlanError::CorruptPlan`] when the stored
/// plan does not match its hash, and [`PlanningError::Persistence`] when the
/// read fails.
pub async fn load_plan(
    conn: &mut PgConnection,
    job_id: JobId,
) -> Result<Option<PlanDocument>, PlanningError> {
    let Some(plan) = plans::plan_for_job(conn, job_id).await? else {
        return Ok(None);
    };
    Ok(Some(PlanDocument::from_persisted(
        plan.content,
        &plan.content_hash,
    )?))
}
