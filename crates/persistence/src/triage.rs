//! Read-only task and attempt rows for job triage (BS-032.T5).
//!
//! The job detail endpoint exposes aggregate counts only. Triage needs the
//! per-task failure and output evidence, so these queries read `tasks`,
//! `attempts`, `attempt_failures`, the worker session an attempt ran on, and
//! its accepted output objects. They are plain pooled reads: immutable rows
//! and snapshots are safe without a transaction or a lock, and no row is ever
//! modified here.

use bergsmith_common::disposition::FailureDisposition;
use bergsmith_common::ids::{AttemptId, JobId, TaskId};
use bergsmith_common::task::{AttemptState, TaskState};
use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::PersistenceError;

/// One task of a job, in plan order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskTriage {
    /// The task identity.
    pub id: TaskId,
    /// The task's position in the plan, from zero.
    pub ordinal: i32,
    /// The durable task state.
    pub state: TaskState,
    /// How many attempts have started, including the current one.
    pub attempts_started: i32,
    /// When a retrying task becomes eligible again.
    pub retry_not_before: Option<DateTime<Utc>>,
    /// When the task row last changed.
    pub updated_at: DateTime<Utc>,
}

/// One attempt of a task, in generation order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptTriage {
    /// The attempt identity.
    pub id: AttemptId,
    /// The task this attempt belongs to.
    pub task_id: TaskId,
    /// The attempt number within its task, from one.
    pub generation: i32,
    /// The durable attempt state.
    pub state: AttemptState,
    /// The worker pool of the session that ran the attempt.
    pub pool: String,
    /// The worker name of the session that ran the attempt.
    pub worker_name: String,
    /// When the grant was recorded, in database time.
    pub granted_at: DateTime<Utc>,
    /// When the lease expires or expired.
    pub lease_expires_at: DateTime<Utc>,
    /// When the attempt finished; absent while it is granted or running.
    pub finished_at: Option<DateTime<Utc>>,
    /// The stable failure reason, when the attempt failed.
    pub failure_reason: Option<String>,
    /// How the coordinator handled the failure.
    pub failure_disposition: Option<FailureDisposition>,
    /// Accepted output objects of this attempt.
    pub output_files: i64,
    /// Accepted output bytes of this attempt.
    pub output_bytes: i64,
}

#[derive(sqlx::FromRow)]
struct TaskRow {
    id: Uuid,
    ordinal: i32,
    state: String,
    attempts_started: i32,
    retry_not_before: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<TaskRow> for TaskTriage {
    type Error = PersistenceError;

    fn try_from(row: TaskRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: TaskId::from_uuid(row.id),
            ordinal: row.ordinal,
            state: TaskState::from_name(&row.state).ok_or_else(|| {
                PersistenceError::CorruptValue {
                    entity: "task state",
                    value: row.state.clone(),
                }
            })?,
            attempts_started: row.attempts_started,
            retry_not_before: row.retry_not_before,
            updated_at: row.updated_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct AttemptRow {
    id: Uuid,
    task_id: Uuid,
    generation: i32,
    state: String,
    pool: String,
    worker_name: String,
    granted_at: DateTime<Utc>,
    lease_expires_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    failure_reason: Option<String>,
    failure_disposition: Option<String>,
    output_files: i64,
    output_bytes: i64,
}

impl TryFrom<AttemptRow> for AttemptTriage {
    type Error = PersistenceError;

    fn try_from(row: AttemptRow) -> Result<Self, Self::Error> {
        let state =
            AttemptState::from_name(&row.state).ok_or_else(|| PersistenceError::CorruptValue {
                entity: "attempt state",
                value: row.state.clone(),
            })?;
        let failure_disposition = row
            .failure_disposition
            .as_deref()
            .map(|name| {
                FailureDisposition::from_name(name).ok_or_else(|| PersistenceError::CorruptValue {
                    entity: "failure disposition",
                    value: name.to_owned(),
                })
            })
            .transpose()?;
        Ok(Self {
            id: AttemptId::from_uuid(row.id),
            task_id: TaskId::from_uuid(row.task_id),
            generation: row.generation,
            state,
            pool: row.pool,
            worker_name: row.worker_name,
            granted_at: row.granted_at,
            lease_expires_at: row.lease_expires_at,
            finished_at: row.finished_at,
            failure_reason: row.failure_reason,
            failure_disposition,
            output_files: row.output_files,
            output_bytes: row.output_bytes,
        })
    }
}

/// A job's tasks in ordinal order, strictly after `after`, up to `limit`.
///
/// # Errors
///
/// [`PersistenceError::Database`] when PostgreSQL rejects the read, and
/// [`PersistenceError::CorruptValue`] when a stored state is unknown.
pub async fn tasks_after(
    conn: &mut PgConnection,
    job_id: JobId,
    after: Option<i32>,
    limit: i64,
) -> Result<Vec<TaskTriage>, PersistenceError> {
    let rows: Vec<TaskRow> = sqlx::query_as(
        "SELECT id, ordinal, state, attempts_started, retry_not_before, updated_at \
         FROM tasks \
         WHERE job_id = $1 AND ($2::int IS NULL OR ordinal > $2) \
         ORDER BY ordinal \
         LIMIT $3",
    )
    .bind(job_id.as_uuid())
    .bind(after)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TaskTriage::try_from).collect()
}

/// The attempts of `tasks`, in task and generation order.
///
/// # Errors
///
/// [`PersistenceError::Database`] when PostgreSQL rejects the read, and
/// [`PersistenceError::CorruptValue`] when a stored state or disposition is
/// unknown.
pub async fn attempts_for(
    conn: &mut PgConnection,
    tasks: &[TaskId],
) -> Result<Vec<AttemptTriage>, PersistenceError> {
    if tasks.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<Uuid> = tasks.iter().map(|task| *task.as_uuid()).collect();
    let rows: Vec<AttemptRow> = sqlx::query_as(
        "SELECT a.id, a.task_id, a.generation, a.state, \
                ws.pool, ws.worker_name, \
                a.granted_at, a.lease_expires_at, a.finished_at, \
                af.reason_code AS failure_reason, af.disposition AS failure_disposition, \
                COALESCE(o.files, 0)::bigint AS output_files, \
                COALESCE(o.bytes, 0)::bigint AS output_bytes \
         FROM attempts a \
         JOIN worker_sessions ws ON ws.id = a.worker_session_id \
         LEFT JOIN attempt_failures af ON af.attempt_id = a.id \
         LEFT JOIN LATERAL ( \
             SELECT count(*) AS files, COALESCE(sum(ob.size_bytes), 0) AS bytes \
             FROM attempt_output_objects ob WHERE ob.attempt_id = a.id \
         ) o ON true \
         WHERE a.task_id = ANY($1) \
         ORDER BY a.task_id, a.generation",
    )
    .bind(&ids)
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(AttemptTriage::try_from).collect()
}
