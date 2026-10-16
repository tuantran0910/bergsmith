//! Execution attempts, leases, and receipt acceptance.
//!
//! Every function here locks the job row first, then the task row, so
//! attempts serialize against job transitions and cancellation in one order.

use std::time::Duration;

use bergsmith_common::ids::{AttemptId, JobId, TaskId, WorkerSessionId};
use bergsmith_common::job::JobState;
use bergsmith_common::task::{AttemptState, AttemptTransitionError, TaskState, retry_allowed};
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::PersistenceError;
use crate::jobs::lock_job;
use crate::plans::{Task, TaskRow, task_columns};

/// One execution attempt of a task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// Identifier.
    pub id: AttemptId,
    /// The task being executed.
    pub task_id: TaskId,
    /// Attempt number for the task, from 1. Old generations can never renew
    /// a lease or supply a result.
    pub generation: i32,
    /// Current state.
    pub state: AttemptState,
    /// The worker session holding the grant.
    pub worker_session_id: WorkerSessionId,
    /// When the grant was persisted (database time).
    pub granted_at: DateTime<Utc>,
    /// When the lease lapses unless renewed (database time).
    pub lease_expires_at: DateTime<Utc>,
    /// When the attempt left `Granted` or `Running`.
    pub finished_at: Option<DateTime<Utc>>,
}

/// Fields for a new grant.
#[derive(Clone, Debug)]
pub struct NewGrant {
    /// Identifier to assign to the attempt.
    pub attempt_id: AttemptId,
    /// The task to grant; it must be `Pending`.
    pub task_id: TaskId,
    /// The worker session receiving the grant.
    pub worker_session_id: WorkerSessionId,
    /// Lease length, measured from database time.
    pub lease: Duration,
}

/// How an attempt ends without an accepted result.
#[derive(Clone, Debug)]
pub struct EndAttempt {
    /// The attempt to end.
    pub attempt_id: AttemptId,
    /// The generation the caller believes is current.
    pub generation: i32,
    /// `Failed`, `Expired`, or `Cancelled`.
    pub attempt_state: AttemptState,
    /// The task's next state: `RetryWaiting`, `Failed`, or `Cancelled`.
    pub task_state: TaskState,
    /// Earliest retry time; required exactly when `task_state` is `RetryWaiting`.
    pub retry_not_before: Option<DateTime<Utc>>,
}

/// A worker's result for an attempt.
#[derive(Clone, Debug)]
pub struct ReceiptSubmission {
    /// The attempt that produced the result.
    pub attempt_id: AttemptId,
    /// The generation the worker was granted.
    pub generation: i32,
    /// Canonical receipt content.
    pub content: serde_json::Value,
    /// Lowercase hex SHA-256 of the canonical content.
    pub content_hash: String,
}

/// The outcome of a receipt submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcceptOutcome {
    /// The receipt is now durable and the task is `Accepted`.
    Accepted,
    /// The identical receipt was accepted earlier; nothing changed.
    AlreadyAccepted,
    /// The attempt is no longer authoritative: superseded, ended, expired, or
    /// fenced by a job that stopped for another reason.
    StaleAttempt,
    /// The job is cancelling or cancelled, so the result was not accepted.
    Cancelled,
}

/// The result of a receipt submission, with the job's current state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Acceptance {
    /// What happened to the receipt.
    pub outcome: AcceptOutcome,
    /// The job's state after the submission.
    pub job_state: JobState,
}

#[derive(sqlx::FromRow)]
struct AttemptRow {
    id: Uuid,
    task_id: Uuid,
    generation: i32,
    state: String,
    worker_session_id: Uuid,
    granted_at: DateTime<Utc>,
    lease_expires_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
}

impl TryFrom<AttemptRow> for Attempt {
    type Error = PersistenceError;

    fn try_from(row: AttemptRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: AttemptId::from_uuid(row.id),
            task_id: TaskId::from_uuid(row.task_id),
            generation: row.generation,
            state: AttemptState::from_name(&row.state).ok_or_else(|| {
                PersistenceError::CorruptValue {
                    entity: "attempt state",
                    value: row.state.clone(),
                }
            })?,
            worker_session_id: WorkerSessionId::from_uuid(row.worker_session_id),
            granted_at: row.granted_at,
            lease_expires_at: row.lease_expires_at,
            finished_at: row.finished_at,
        })
    }
}

macro_rules! attempt_columns {
    () => {
        "id, task_id, generation, state, worker_session_id, granted_at, lease_expires_at, \
         finished_at"
    };
}

async fn job_of_task(conn: &mut PgConnection, task_id: TaskId) -> Result<JobId, PersistenceError> {
    sqlx::query_scalar::<_, Uuid>("SELECT job_id FROM tasks WHERE id = $1")
        .bind(task_id.as_uuid())
        .fetch_optional(conn)
        .await?
        .map(JobId::from_uuid)
        .ok_or_else(|| PersistenceError::NotFound {
            entity: "task",
            id: task_id.to_string(),
        })
}

async fn task_of_attempt(
    conn: &mut PgConnection,
    attempt_id: AttemptId,
) -> Result<TaskId, PersistenceError> {
    sqlx::query_scalar::<_, Uuid>("SELECT task_id FROM attempts WHERE id = $1")
        .bind(attempt_id.as_uuid())
        .fetch_optional(conn)
        .await?
        .map(TaskId::from_uuid)
        .ok_or_else(|| PersistenceError::NotFound {
            entity: "attempt",
            id: attempt_id.to_string(),
        })
}

async fn lock_task(conn: &mut PgConnection, id: TaskId) -> Result<Task, PersistenceError> {
    sqlx::query_as::<_, TaskRow>(concat!(
        "SELECT ",
        task_columns!(),
        " FROM tasks WHERE id = $1 FOR UPDATE"
    ))
    .bind(id.as_uuid())
    .fetch_one(conn)
    .await?
    .try_into()
}

/// Locks an attempt and reports whether its lease is still live by database time.
async fn lock_attempt(
    conn: &mut PgConnection,
    id: AttemptId,
) -> Result<(Attempt, bool), PersistenceError> {
    #[derive(sqlx::FromRow)]
    struct Locked {
        #[sqlx(flatten)]
        attempt: AttemptRow,
        lease_live: bool,
    }
    let locked = sqlx::query_as::<_, Locked>(concat!(
        "SELECT ",
        attempt_columns!(),
        ", lease_expires_at > now() AS lease_live FROM attempts WHERE id = $1 FOR UPDATE"
    ))
    .bind(id.as_uuid())
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "attempt",
        id: id.to_string(),
    })?;
    Ok((locked.attempt.try_into()?, locked.lease_live))
}

async fn set_task_state(
    conn: &mut PgConnection,
    task_id: TaskId,
    state: TaskState,
    retry_not_before: Option<DateTime<Utc>>,
) -> Result<(), PersistenceError> {
    sqlx::query(
        "UPDATE tasks SET state = $2, retry_not_before = $3, updated_at = now() WHERE id = $1",
    )
    .bind(task_id.as_uuid())
    .bind(state.as_str())
    .bind(retry_not_before)
    .execute(conn)
    .await
    .map_err(|error| PersistenceError::from_write("task", error))?;
    Ok(())
}

async fn finish_attempt(
    conn: &mut PgConnection,
    attempt_id: AttemptId,
    state: AttemptState,
) -> Result<Attempt, PersistenceError> {
    sqlx::query_as::<_, AttemptRow>(concat!(
        "UPDATE attempts SET state = $2, finished_at = now() WHERE id = $1 RETURNING ",
        attempt_columns!()
    ))
    .bind(attempt_id.as_uuid())
    .bind(state.as_str())
    .fetch_one(conn)
    .await?
    .try_into()
}

/// Persists a grant: a new attempt with the next generation and a lease, and
/// the task moved to `Leased`. The grant is durable before it is returned.
///
/// # Errors
///
/// - [`PersistenceError::JobNotInState`] unless the job is `Queued` or `Running`.
/// - [`PersistenceError::TaskTransition`] unless the task is `Pending`.
/// - [`PersistenceError::RetryBudgetExhausted`] when the task has used every
///   execution attempt.
/// - [`PersistenceError::AlreadyExists`] when the task already has an active
///   attempt.
pub async fn grant_attempt(
    conn: &mut PgConnection,
    grant: &NewGrant,
) -> Result<Attempt, PersistenceError> {
    let mut tx = conn.begin().await?;
    let job_id = job_of_task(&mut tx, grant.task_id).await?;
    let job = lock_job(&mut tx, job_id).await?;
    if !matches!(job.state, JobState::Queued | JobState::Running) {
        return Err(PersistenceError::JobNotInState {
            job: job.id,
            state: job.state,
            operation: "grant an attempt",
        });
    }
    let task = lock_task(&mut tx, grant.task_id).await?;
    task.state.validate_transition(TaskState::Leased)?;
    let started = u32::try_from(task.attempts_started).unwrap_or(u32::MAX);
    if !retry_allowed(started) {
        return Err(PersistenceError::RetryBudgetExhausted { task: task.id });
    }
    let lease_ms = i64::try_from(grant.lease.as_millis()).unwrap_or(i64::MAX);

    let attempt: Attempt = sqlx::query_as::<_, AttemptRow>(concat!(
        "INSERT INTO attempts (id, task_id, generation, state, worker_session_id, lease_expires_at)
         VALUES ($1, $2, $3, $4, $5, now() + $6 * interval '1 millisecond')
         RETURNING ",
        attempt_columns!()
    ))
    .bind(grant.attempt_id.as_uuid())
    .bind(grant.task_id.as_uuid())
    .bind(task.attempts_started + 1)
    .bind(AttemptState::Granted.as_str())
    .bind(grant.worker_session_id.as_uuid())
    .bind(lease_ms)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("attempt", error))?
    .try_into()?;

    sqlx::query(
        "UPDATE tasks SET state = $2, attempts_started = $3, retry_not_before = NULL,
             updated_at = now()
         WHERE id = $1",
    )
    .bind(task.id.as_uuid())
    .bind(TaskState::Leased.as_str())
    .bind(attempt.generation)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(attempt)
}

/// Ends an active attempt without a result and moves its task on.
///
/// # Errors
///
/// - [`PersistenceError::StaleAttempt`] when `generation` is not the attempt's.
/// - [`PersistenceError::AttemptTransition`] when the attempt already ended or
///   `attempt_state` is not an ending without a result.
/// - [`PersistenceError::TaskTransition`] when the task cannot take `task_state`.
/// - [`PersistenceError::InvalidValue`] when `retry_not_before` is set for any
///   state other than `RetryWaiting`, or missing for it.
pub async fn end_attempt(
    conn: &mut PgConnection,
    end: &EndAttempt,
) -> Result<Attempt, PersistenceError> {
    let mut tx = conn.begin().await?;
    let task_id = task_of_attempt(&mut tx, end.attempt_id).await?;
    let job_id = job_of_task(&mut tx, task_id).await?;
    lock_job(&mut tx, job_id).await?;
    let (attempt, _) = lock_attempt(&mut tx, end.attempt_id).await?;
    if attempt.generation != end.generation {
        return Err(PersistenceError::StaleAttempt {
            attempt: attempt.id,
        });
    }
    // Accepting needs a receipt; use `accept_receipt`.
    if end.attempt_state == AttemptState::Accepted {
        return Err(PersistenceError::AttemptTransition(
            AttemptTransitionError {
                from: attempt.state,
                to: end.attempt_state,
            },
        ));
    }
    attempt.state.validate_transition(end.attempt_state)?;
    let task = lock_task(&mut tx, attempt.task_id).await?;
    task.state.validate_transition(end.task_state)?;

    let ended = finish_attempt(&mut tx, attempt.id, end.attempt_state).await?;
    set_task_state(&mut tx, task.id, end.task_state, end.retry_not_before).await?;
    tx.commit().await?;
    Ok(ended)
}

/// Validates and durably accepts a receipt, serialized against job
/// transitions, cancellation, and lease expiry through the job row lock.
///
/// An exact replay of an accepted receipt returns
/// [`AcceptOutcome::AlreadyAccepted`] without reviving the attempt or
/// restoring publication authority.
///
/// # Errors
///
/// - [`PersistenceError::ReceiptConflict`] when the attempt's accepted receipt
///   has different content.
/// - [`PersistenceError::InvalidValue`] for a malformed content hash.
/// - [`PersistenceError::NotFound`] when the attempt does not exist.
pub async fn accept_receipt(
    conn: &mut PgConnection,
    submission: &ReceiptSubmission,
) -> Result<Acceptance, PersistenceError> {
    let mut tx = conn.begin().await?;
    let task_id = task_of_attempt(&mut tx, submission.attempt_id).await?;
    let job_id = job_of_task(&mut tx, task_id).await?;
    let job = lock_job(&mut tx, job_id).await?;
    let task = lock_task(&mut tx, task_id).await?;
    let (attempt, lease_live) = lock_attempt(&mut tx, submission.attempt_id).await?;
    let respond = |outcome| Acceptance {
        outcome,
        job_state: job.state,
    };

    let accepted: Option<(Uuid, String)> =
        sqlx::query_as("SELECT attempt_id, content_hash FROM receipts WHERE task_id = $1")
            .bind(task.id.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
    if let Some((accepted_attempt, accepted_hash)) = accepted {
        if accepted_attempt != *attempt.id.as_uuid() {
            return Ok(respond(AcceptOutcome::StaleAttempt));
        }
        if accepted_hash != submission.content_hash {
            return Err(PersistenceError::ReceiptConflict { task: task.id });
        }
        return Ok(respond(AcceptOutcome::AlreadyAccepted));
    }

    if matches!(job.state, JobState::Cancelling | JobState::Cancelled) {
        return Ok(respond(AcceptOutcome::Cancelled));
    }
    let authoritative = job.state == JobState::Running
        && attempt.generation == submission.generation
        && !attempt.state.is_terminal()
        && lease_live;
    if !authoritative {
        return Ok(respond(AcceptOutcome::StaleAttempt));
    }
    task.state.validate_transition(TaskState::Accepted)?;

    sqlx::query(
        "INSERT INTO receipts (attempt_id, task_id, content, content_hash) VALUES ($1, $2, $3, $4)",
    )
    .bind(attempt.id.as_uuid())
    .bind(task.id.as_uuid())
    .bind(&submission.content)
    .bind(&submission.content_hash)
    .execute(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("receipt", error))?;
    finish_attempt(&mut tx, attempt.id, AttemptState::Accepted).await?;
    set_task_state(&mut tx, task.id, TaskState::Accepted, None).await?;
    tx.commit().await?;
    Ok(respond(AcceptOutcome::Accepted))
}

/// Reads one attempt.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the attempt does not exist.
pub async fn attempt(conn: &mut PgConnection, id: AttemptId) -> Result<Attempt, PersistenceError> {
    sqlx::query_as::<_, AttemptRow>(concat!(
        "SELECT ",
        attempt_columns!(),
        " FROM attempts WHERE id = $1"
    ))
    .bind(id.as_uuid())
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "attempt",
        id: id.to_string(),
    })?
    .try_into()
}
