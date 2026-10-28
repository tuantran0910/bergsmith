//! Immutable plans of metadata-only `main` rewrites (BS-031.T3, ADR 0063).
//!
//! Manifest rewrite and dangling-delete removal have no tasks: a durable plan
//! moves a `Planning` job straight to `ReadyToCommit`, and the job publishes
//! through the ordinary publication intent.

use bergsmith_common::ids::{JobId, OrganizationId, PlanId, TableId};
use bergsmith_common::job::JobState;
use sqlx::{Connection, PgConnection};

use crate::PersistenceError;
use crate::jobs::{self, Job, Transition};

/// Fields for a new metadata plan.
#[derive(Clone, Debug)]
pub struct NewMetadataPlan<'a> {
    /// Identifier to assign.
    pub id: PlanId,
    /// The `RewriteManifests` or `RemoveDanglingDeletes` job.
    pub job_id: JobId,
    /// The plan document, including the baseline it was computed from.
    pub content: &'a serde_json::Value,
    /// Lowercase hex SHA-256 of `content`.
    pub content_hash: &'a str,
}

/// Persists a metadata plan and moves its job to `ReadyToCommit`.
///
/// The plan row and the transition share one transaction, with the job row
/// locked and the table's planning lock held, as for expiration plans.
///
/// # Errors
///
/// - [`PersistenceError::InvalidValue`] when the job is not a metadata
///   rewrite, or the hash is malformed.
/// - [`PersistenceError::AlreadyExists`] when the job already has a plan.
/// - [`PersistenceError::JobNotInState`] when the job is not `Planning`.
/// - [`PersistenceError::RevisionConflict`] when another writer changed the
///   job since the caller read it.
/// - [`PersistenceError::NotFound`] when the job does not exist, and
///   leadership errors when `epoch` is no longer current.
pub async fn create_metadata_plan(
    conn: &mut PgConnection,
    plan: &NewMetadataPlan<'_>,
    expected_revision: i64,
    epoch: i64,
) -> Result<Job, PersistenceError> {
    let mut tx = conn.begin().await?;
    let job = jobs::lock_job(&mut tx, plan.job_id).await?;
    if !job.operation.is_metadata_rewrite() {
        return Err(PersistenceError::InvalidValue {
            entity: "metadata plan",
            constraint: "a metadata plan requires a RewriteManifests or RemoveDanglingDeletes job"
                .into(),
        });
    }
    if job.state != JobState::Planning {
        return Err(PersistenceError::JobNotInState {
            job: job.id,
            state: job.state,
            operation: "create a metadata plan",
        });
    }
    crate::plans::lock_table_planning(&mut tx, job.table_id).await?;
    sqlx::query(
        "INSERT INTO metadata_plans (id, job_id, content, content_hash)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(plan.id.as_uuid())
    .bind(plan.job_id.as_uuid())
    .bind(plan.content)
    .bind(plan.content_hash)
    .execute(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("metadata plan", error))?;
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
    crate::leadership::ensure_leadership(&mut tx, epoch).await?;
    tx.commit().await?;
    Ok(transitioned)
}

/// Reads a job's metadata plan as its content and hash.
///
/// # Errors
///
/// Returns a database error when the query fails.
pub async fn metadata_plan(
    conn: &mut PgConnection,
    job_id: JobId,
) -> Result<Option<(serde_json::Value, String)>, PersistenceError> {
    Ok(sqlx::query_as::<_, (serde_json::Value, String)>(
        "SELECT content, content_hash FROM metadata_plans WHERE job_id = $1",
    )
    .bind(job_id.as_uuid())
    .fetch_optional(conn)
    .await?)
}

/// Whether the table has a compaction job that has not finished.
///
/// A metadata rewrite publishes a new `main` snapshot, which would send such a
/// compaction back to planning after its workers rewrote data, so submission
/// refuses one while this holds (ADR 0063).
///
/// # Errors
///
/// Returns a database error when the query fails.
pub async fn compaction_in_progress(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_id: TableId,
) -> Result<bool, PersistenceError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM jobs
             WHERE organization_id = $1 AND table_identity_id = $2
               AND operation = 'Compact'
               AND state NOT IN ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed'))",
    )
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .fetch_one(conn)
    .await?)
}
