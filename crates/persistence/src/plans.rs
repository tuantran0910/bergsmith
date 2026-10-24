//! Immutable plans and the tasks they define.

use bergsmith_common::ids::{JobId, PlanId, TaskId};
use bergsmith_common::job::JobState;
use bergsmith_common::task::TaskState;
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::PersistenceError;
use crate::jobs::lock_job;

/// Locks a planning job and its live coordinator epoch in the caller's transaction.
///
/// Keep this transaction open through recording the outcome. Lock order is
/// job, then leadership, matching admission and publication.
///
/// # Errors
/// Returns lost leadership, a changed revision/state, or a database error.
pub async fn lock_for_planning(
    tx: &mut Transaction<'_, Postgres>,
    job_id: JobId,
    revision: i64,
    epoch: i64,
) -> Result<crate::jobs::Job, PersistenceError> {
    let job = lock_job(tx, job_id).await?;
    crate::leadership::ensure_leadership(tx, epoch).await?;
    if job.state_revision != revision {
        return Err(PersistenceError::RevisionConflict {
            job: job_id,
            expected: revision,
            actual: job.state_revision,
        });
    }
    if job.state != JobState::Planning {
        return Err(PersistenceError::JobNotInState {
            job: job_id,
            state: job.state,
            operation: "record a planning outcome",
        });
    }
    Ok(job)
}

/// A job's immutable execution plan. Once written it never changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Identifier.
    pub id: PlanId,
    /// The job the plan belongs to. A job has at most one plan.
    pub job_id: JobId,
    /// Canonical plan content.
    pub content: serde_json::Value,
    /// Lowercase hex SHA-256 of the canonical content.
    pub content_hash: String,
    /// When the plan was persisted (database time).
    pub created_at: DateTime<Utc>,
}

/// Fields for a new plan and its tasks.
#[derive(Clone, Debug)]
pub struct NewPlan {
    /// Identifier to assign.
    pub id: PlanId,
    /// The job, which must still be in [`JobState::Planning`].
    pub job_id: JobId,
    /// Canonical plan content.
    pub content: serde_json::Value,
    /// Lowercase hex SHA-256 of the canonical content.
    pub content_hash: String,
    /// One identifier per task; the position becomes the task's ordinal.
    pub task_ids: Vec<TaskId>,
    /// How long the whole job may run, counted from the moment the plan is
    /// persisted.
    ///
    /// The plan is the job's execution contract, so the deadline belongs with
    /// it: every attempt and every retry must fit before it. The deadline is
    /// computed by the database clock, never by the caller's.
    pub execution_budget: std::time::Duration,
}

/// One task of a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    /// Identifier.
    pub id: TaskId,
    /// The job the task belongs to.
    pub job_id: JobId,
    /// The plan that defines the task.
    pub plan_id: PlanId,
    /// Position within the plan, from 0.
    pub ordinal: i32,
    /// Current state.
    pub state: TaskState,
    /// Execution attempts started so far, including the original.
    pub attempts_started: i32,
    /// Earliest retry time while the task is [`TaskState::RetryWaiting`].
    pub retry_not_before: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct PlanRow {
    id: Uuid,
    job_id: Uuid,
    content: serde_json::Value,
    content_hash: String,
    created_at: DateTime<Utc>,
}

impl From<PlanRow> for Plan {
    fn from(row: PlanRow) -> Self {
        Self {
            id: PlanId::from_uuid(row.id),
            job_id: JobId::from_uuid(row.job_id),
            content: row.content,
            content_hash: row.content_hash,
            created_at: row.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
pub(crate) struct TaskRow {
    id: Uuid,
    job_id: Uuid,
    plan_id: Uuid,
    ordinal: i32,
    state: String,
    attempts_started: i32,
    retry_not_before: Option<DateTime<Utc>>,
}

impl TryFrom<TaskRow> for Task {
    type Error = PersistenceError;

    fn try_from(row: TaskRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: TaskId::from_uuid(row.id),
            job_id: JobId::from_uuid(row.job_id),
            plan_id: PlanId::from_uuid(row.plan_id),
            ordinal: row.ordinal,
            state: TaskState::from_name(&row.state).ok_or_else(|| {
                PersistenceError::CorruptValue {
                    entity: "task state",
                    value: row.state.clone(),
                }
            })?,
            attempts_started: row.attempts_started,
            retry_not_before: row.retry_not_before,
        })
    }
}

/// Columns selected for [`Task`], in [`TaskRow`] order.
macro_rules! task_columns {
    () => {
        "id, job_id, plan_id, ordinal, state, attempts_started, retry_not_before"
    };
}
pub(crate) use task_columns;

/// Persists a job's plan and its tasks, all `Pending`, in one transaction.
///
/// # Errors
///
/// - [`PersistenceError::JobNotInState`] unless the job is `Planning`; a
///   queued, running, or terminal job can never receive a different plan.
/// - [`PersistenceError::AlreadyExists`] when the job already has a plan.
/// - [`PersistenceError::InvalidValue`] for a malformed content hash.
/// - [`PersistenceError::NotFound`] when the job does not exist.
pub async fn create_plan(conn: &mut PgConnection, new: &NewPlan) -> Result<Plan, PersistenceError> {
    let mut tx = conn.begin().await?;
    let job = lock_job(&mut tx, new.job_id).await?;
    if job.state != JobState::Planning {
        return Err(PersistenceError::JobNotInState {
            job: job.id,
            state: job.state,
            operation: "accept a plan",
        });
    }
    let plan: Plan = sqlx::query_as::<_, PlanRow>(
        "INSERT INTO plans (id, job_id, content, content_hash) VALUES ($1, $2, $3, $4)
         RETURNING id, job_id, content, content_hash, created_at",
    )
    .bind(new.id.as_uuid())
    .bind(new.job_id.as_uuid())
    .bind(&new.content)
    .bind(&new.content_hash)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("plan", error))?
    .into();

    // The deadline is written with the plan, from the database clock: the
    // budget covers the whole job from the moment its baseline was captured.
    let budget_ms = i64::try_from(new.execution_budget.as_millis()).unwrap_or(i64::MAX);
    sqlx::query(
        "UPDATE jobs SET deadline_at = clock_timestamp() + $2 * interval '1 millisecond'
         WHERE id = $1 AND deadline_at IS NULL",
    )
    .bind(new.job_id.as_uuid())
    .bind(budget_ms)
    .execute(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("job", error))?;

    let ids: Vec<Uuid> = new.task_ids.iter().map(|id| *id.as_uuid()).collect();
    sqlx::query(
        "INSERT INTO tasks (id, job_id, plan_id, ordinal, state)
         SELECT task.id, $2, $3, (task.ordinality - 1)::integer, $4
         FROM unnest($1::uuid[]) WITH ORDINALITY AS task (id, ordinality)",
    )
    .bind(&ids)
    .bind(new.job_id.as_uuid())
    .bind(new.id.as_uuid())
    .bind(TaskState::Pending.as_str())
    .execute(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("task", error))?;

    tx.commit().await?;
    Ok(plan)
}

/// Reads a job's plan, if it has one.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn plan_for_job(
    conn: &mut PgConnection,
    job_id: JobId,
) -> Result<Option<Plan>, PersistenceError> {
    let row = sqlx::query_as::<_, PlanRow>(
        "SELECT id, job_id, content, content_hash, created_at FROM plans WHERE job_id = $1",
    )
    .bind(job_id.as_uuid())
    .fetch_optional(conn)
    .await?;
    Ok(row.map(Plan::from))
}

/// Returns a plan's tasks in ordinal order.
///
/// # Errors
///
/// [`PersistenceError::CorruptValue`] when a stored task does not decode.
pub async fn tasks(
    conn: &mut PgConnection,
    plan_id: PlanId,
) -> Result<Vec<Task>, PersistenceError> {
    sqlx::query_as::<_, TaskRow>(concat!(
        "SELECT ",
        task_columns!(),
        " FROM tasks WHERE plan_id = $1 ORDER BY ordinal"
    ))
    .bind(plan_id.as_uuid())
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(Task::try_from)
    .collect()
}

/// Reads one task.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the task does not exist.
pub async fn task(conn: &mut PgConnection, id: TaskId) -> Result<Task, PersistenceError> {
    sqlx::query_as::<_, TaskRow>(concat!(
        "SELECT ",
        task_columns!(),
        " FROM tasks WHERE id = $1"
    ))
    .bind(id.as_uuid())
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "task",
        id: id.to_string(),
    })?
    .try_into()
}
