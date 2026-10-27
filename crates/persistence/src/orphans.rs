//! Immutable orphan-cleanup plans and per-object deletion receipts
//! (BS-031.T2, ADR 0062).
//!
//! Orphan cleanup is a coordinator-only operation: its plan names the storage
//! objects to delete, so it has no tasks and moves a `Planning` job straight to
//! `ReadyToCommit`. Deletion then proceeds in bounded batches from
//! `Deleting`, and each candidate gets one immutable receipt.

use std::collections::BTreeSet;

use bergsmith_common::ids::{JobId, PlanId};
use bergsmith_common::job::{JobOperation, JobState};
use sqlx::{Connection, PgConnection};

use crate::error::PersistenceError;
use crate::jobs::{self, Job, Transition};

/// Fields for a new orphan plan.
#[derive(Clone, Copy, Debug)]
pub struct NewOrphanPlan<'a> {
    /// The plan identifier.
    pub id: PlanId,
    /// The owning job.
    pub job_id: JobId,
    /// The canonical plan document.
    pub content: &'a serde_json::Value,
    /// The hex SHA-256 of the canonical document.
    pub content_hash: &'a str,
}

/// How one planned candidate was resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// The object was removed.
    Deleted,
    /// Re-verification kept it, so nothing was removed.
    Skipped,
}

impl Disposition {
    /// The persisted name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deleted => "Deleted",
            Self::Skipped => "Skipped",
        }
    }
}

/// One receipt to record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt<'a> {
    /// The object's full location.
    pub path: &'a str,
    /// What happened to it.
    pub disposition: Disposition,
    /// The listed size in bytes.
    pub size_bytes: u64,
}

/// Persists an orphan plan and moves its job to `ReadyToCommit`.
///
/// Both writes happen in one transaction: the plan row is inserted and the job
/// is revision-checked and transitioned while its row is locked. The job has no
/// tasks, so nothing stands between a durable plan and deletion.
///
/// # Errors
///
/// - [`PersistenceError::InvalidValue`] when the job is not a
///   `RemoveOrphanFiles` job, or the hash is malformed.
/// - [`PersistenceError::AlreadyExists`] when the job already has a plan.
/// - [`PersistenceError::RevisionConflict`] when another writer changed the
///   job since the caller read it.
/// - [`PersistenceError::JobTransition`] when the lifecycle rules forbid
///   `Planning -> ReadyToCommit`.
/// - [`PersistenceError::NotFound`] when the job does not exist.
pub async fn create_orphan_plan(
    conn: &mut PgConnection,
    plan: &NewOrphanPlan<'_>,
    expected_revision: i64,
    epoch: i64,
) -> Result<Job, PersistenceError> {
    let mut tx = conn.begin().await?;
    let job = jobs::lock_job(&mut tx, plan.job_id).await?;
    if job.operation != JobOperation::RemoveOrphanFiles {
        return Err(PersistenceError::InvalidValue {
            entity: "orphan plan",
            constraint: "an orphan plan requires a RemoveOrphanFiles job".into(),
        });
    }
    if job.state != JobState::Planning {
        return Err(PersistenceError::JobNotInState {
            job: job.id,
            state: job.state,
            operation: "create an orphan plan",
        });
    }
    crate::plans::lock_table_planning(&mut tx, job.table_id).await?;
    sqlx::query(
        "INSERT INTO orphan_plans (id, job_id, content, content_hash)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(plan.id.as_uuid())
    .bind(plan.job_id.as_uuid())
    .bind(plan.content)
    .bind(plan.content_hash)
    .execute(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("orphan plan", error))?;
    crate::leadership::ensure_leadership(&mut tx, epoch).await?;
    let transitioned = jobs::transition_job_within(
        &mut tx,
        &Transition {
            job_id: job.id,
            expected_revision,
            to: JobState::ReadyToCommit,
            reason: None,
            actor: None,
            non_publication_proven: false,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(transitioned)
}

/// Reads a job's orphan plan as its content and hash.
///
/// # Errors
///
/// Returns a database error when the query fails.
pub async fn orphan_plan(
    conn: &mut PgConnection,
    job_id: JobId,
) -> Result<Option<(serde_json::Value, String)>, PersistenceError> {
    Ok(sqlx::query_as::<_, (serde_json::Value, String)>(
        "SELECT content, content_hash FROM orphan_plans WHERE job_id = $1",
    )
    .bind(job_id.as_uuid())
    .fetch_optional(conn)
    .await?)
}

/// Moves a job from `ReadyToCommit` to `Deleting` under the leader lease.
///
/// # Errors
///
/// [`PersistenceError::RevisionConflict`] when another writer changed the job,
/// [`PersistenceError::JobTransition`] when the lifecycle forbids the move, and
/// [`PersistenceError::LeadershipLost`] when the lease is gone.
pub async fn begin_deletion(
    conn: &mut PgConnection,
    job_id: JobId,
    expected_revision: i64,
    epoch: i64,
) -> Result<Job, PersistenceError> {
    let mut tx = conn.begin().await?;
    let job = jobs::lock_job(&mut tx, job_id).await?;
    if job.state != JobState::ReadyToCommit {
        return Err(PersistenceError::JobNotInState {
            job: job.id,
            state: job.state,
            operation: "begin orphan deletion",
        });
    }
    crate::leadership::ensure_leadership(&mut tx, epoch).await?;
    let transitioned = jobs::transition_job_within(
        &mut tx,
        &Transition {
            job_id,
            expected_revision,
            to: JobState::Deleting,
            reason: Some("DeletionAuthorized".to_owned()),
            actor: None,
            non_publication_proven: false,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(transitioned)
}

/// Records one receipt per resolved candidate, ignoring duplicates.
///
/// A retry after a crash between the object delete and the receipt insert sees
/// the same path again; `ON CONFLICT DO NOTHING` keeps the first row and lets
/// the pass continue.
///
/// # Errors
///
/// Returns a database error when the insert fails.
pub async fn record_receipts(
    conn: &mut PgConnection,
    job_id: JobId,
    receipts: &[Receipt<'_>],
) -> Result<(), PersistenceError> {
    if receipts.is_empty() {
        return Ok(());
    }
    let mut tx = conn.begin().await?;
    for receipt in receipts {
        sqlx::query(
            "INSERT INTO orphan_deletions (job_id, path, disposition, size_bytes)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (job_id, path) DO NOTHING",
        )
        .bind(job_id.as_uuid())
        .bind(receipt.path)
        .bind(receipt.disposition.as_str())
        .bind(i64::try_from(receipt.size_bytes).unwrap_or(i64::MAX))
        .execute(&mut *tx)
        .await
        .map_err(|error| PersistenceError::from_write("orphan deletion receipt", error))?;
    }
    tx.commit().await?;
    Ok(())
}

/// The paths that already have a receipt, so a resumed pass skips them.
///
/// # Errors
///
/// Returns a database error when the query fails.
pub async fn recorded_paths(
    conn: &mut PgConnection,
    job_id: JobId,
) -> Result<BTreeSet<String>, PersistenceError> {
    let paths: Vec<String> =
        sqlx::query_scalar("SELECT path FROM orphan_deletions WHERE job_id = $1")
            .bind(job_id.as_uuid())
            .fetch_all(conn)
            .await?;
    Ok(paths.into_iter().collect())
}

/// The number of receipts and the bytes they deleted.
///
/// # Errors
///
/// Returns a database error when the query fails.
pub async fn receipt_totals(
    conn: &mut PgConnection,
    job_id: JobId,
) -> Result<(i64, i64), PersistenceError> {
    Ok(sqlx::query_as(
        "SELECT count(*),
                COALESCE(sum(size_bytes) FILTER (WHERE disposition = 'Deleted'), 0)::bigint
         FROM orphan_deletions WHERE job_id = $1",
    )
    .bind(job_id.as_uuid())
    .fetch_one(conn)
    .await?)
}

/// Fences one deletion batch under the leader lease.
///
/// Deletion is irreversible, so a resumed pass must prove it still holds the
/// lease and that nobody cancelled or ended the job since the page was read.
/// The returned job carries the revision the batch is allowed to complete at.
///
/// # Errors
///
/// [`PersistenceError::JobNotInState`] when the job is no longer `Deleting` or
/// a cancellation was recorded, and
/// [`PersistenceError::LeadershipLost`] when the lease is gone.
pub async fn authorize_batch(
    conn: &mut PgConnection,
    job_id: JobId,
    epoch: i64,
) -> Result<Job, PersistenceError> {
    let mut tx = conn.begin().await?;
    crate::leadership::ensure_leadership(&mut tx, epoch).await?;
    let job = jobs::lock_job(&mut tx, job_id).await?;
    if job.state != JobState::Deleting || job.cancellation.is_some() {
        return Err(PersistenceError::JobNotInState {
            job: job.id,
            state: job.state,
            operation: "authorize an orphan deletion batch",
        });
    }
    tx.commit().await?;
    Ok(job)
}

/// Ends a deletion in `to` under the leader lease, in one transaction.
///
/// The leadership check and the transition commit together, so a coordinator
/// that lost the lease cannot end the job.
///
/// # Errors
///
/// [`PersistenceError::RevisionConflict`] when another writer changed the job,
/// [`PersistenceError::JobTransition`] when the lifecycle forbids the move, and
/// [`PersistenceError::LeadershipLost`] when the lease is gone.
pub async fn end_deletion(
    conn: &mut PgConnection,
    job_id: JobId,
    expected_revision: i64,
    epoch: i64,
    to: JobState,
    reason: &'static str,
) -> Result<Job, PersistenceError> {
    let mut tx = conn.begin().await?;
    crate::leadership::ensure_leadership(&mut tx, epoch).await?;
    let transitioned = jobs::transition_job_within(
        &mut tx,
        &Transition {
            job_id,
            expected_revision,
            to,
            reason: Some(reason.to_owned()),
            actor: None,
            non_publication_proven: false,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(transitioned)
}
