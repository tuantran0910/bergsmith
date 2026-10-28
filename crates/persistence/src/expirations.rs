//! Immutable expiration plans and the snapshots the ledger still pins.
//!
//! Expiration is a metadata-only operation: its plan names the snapshots to
//! remove, so it has no tasks and moves a `Planning` job straight to
//! `ReadyToCommit`.

use std::collections::BTreeSet;

use bergsmith_common::ids::{JobId, OrganizationId, PlanId, TableId};
use bergsmith_common::job::{JobOperation, JobState};
use sqlx::{Connection, PgConnection};

use crate::PersistenceError;
use crate::jobs::{self, Job, Transition, lock_job};

/// Fields for a new expiration plan.
#[derive(Clone, Debug)]
pub struct NewExpirationPlan<'a> {
    /// Identifier to assign.
    pub id: PlanId,
    /// The `ExpireSnapshots` job the plan belongs to.
    pub job_id: JobId,
    /// The plan document, including the baseline it was computed from.
    pub content: &'a serde_json::Value,
    /// Lowercase hex SHA-256 of `content`.
    pub content_hash: &'a str,
}

/// Persists an expiration plan and moves its job to `ReadyToCommit`.
///
/// Both writes happen in one transaction: the plan row is inserted and the
/// job is revision-checked and transitioned while its row is locked. The job
/// has no tasks, so nothing stands between a durable plan and commit.
///
/// # Errors
///
/// - [`PersistenceError::InvalidValue`] when the job is not an
///   `ExpireSnapshots` job, or the hash is malformed.
/// - [`PersistenceError::AlreadyExists`] when the job already has an
///   expiration plan.
/// - [`PersistenceError::RevisionConflict`] when another writer changed the
///   job since the caller read it.
/// - [`PersistenceError::JobTransition`] when the lifecycle rules forbid
///   `Planning -> ReadyToCommit`.
/// - [`PersistenceError::NotFound`] when the job does not exist.
pub async fn create_expiration_plan(
    conn: &mut PgConnection,
    plan: &NewExpirationPlan<'_>,
    expected_revision: i64,
    epoch: i64,
) -> Result<Job, PersistenceError> {
    let mut tx = conn.begin().await?;
    let job = lock_job(&mut tx, plan.job_id).await?;
    if job.operation != JobOperation::ExpireSnapshots {
        return Err(PersistenceError::InvalidValue {
            entity: "job operation",
            constraint: "an expiration plan requires an ExpireSnapshots job".into(),
        });
    }
    crate::plans::lock_table_planning(&mut tx, job.table_id).await?;
    sqlx::query(
        "INSERT INTO expiration_plans (id, job_id, content, content_hash)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(plan.id.as_uuid())
    .bind(plan.job_id.as_uuid())
    .bind(plan.content)
    .bind(plan.content_hash)
    .execute(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("expiration plan", error))?;
    let transitioned = jobs::transition_job(
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

/// Reads a job's expiration plan as its content and hash.
///
/// # Errors
///
/// Returns a database error when the query fails.
pub async fn expiration_plan(
    conn: &mut PgConnection,
    job_id: JobId,
) -> Result<Option<(serde_json::Value, String)>, PersistenceError> {
    Ok(sqlx::query_as::<_, (serde_json::Value, String)>(
        "SELECT content, content_hash FROM expiration_plans WHERE job_id = $1",
    )
    .bind(job_id.as_uuid())
    .fetch_optional(conn)
    .await?)
}

/// Snapshot identifiers a live job, plan, or publication still pins.
///
/// Protected snapshots come from every nonterminal job on the table (its
/// execution, expiration, and metadata plan baselines) and from publication
/// intents:
/// rewrite intents pin their plan's baseline, and expiration intents pin their
/// explicit `baseline_snapshot_id` and `protected` entries. The expiring job is
/// excluded, so it can remove what it planned to remove. Only well-formed
/// non-negative decimal strings are cast; malformed and null values are
/// ignored.
///
/// # Errors
///
/// Returns a database error when the query fails.
pub async fn protected_snapshots(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    table_id: TableId,
    exclude_job: JobId,
) -> Result<BTreeSet<i64>, PersistenceError> {
    let snapshots: Vec<i64> = sqlx::query_scalar(
        "WITH pinned AS (
             SELECT p.content -> 'baseline' ->> 'snapshot_id' AS snapshot
             FROM plans p
             JOIN jobs j ON j.id = p.job_id
             WHERE j.organization_id = $1 AND j.table_identity_id = $2 AND j.id <> $3
               AND j.state NOT IN
                   ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed')
             UNION ALL
             SELECT e.content -> 'baseline' ->> 'snapshot_id'
             FROM expiration_plans e
             JOIN jobs j ON j.id = e.job_id
             WHERE j.organization_id = $1 AND j.table_identity_id = $2 AND j.id <> $3
               AND j.state NOT IN
                   ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed')
             UNION ALL
             SELECT m.content -> 'baseline' ->> 'snapshot_id'
             FROM metadata_plans m
             JOIN jobs j ON j.id = m.job_id
             WHERE j.organization_id = $1 AND j.table_identity_id = $2 AND j.id <> $3
               AND j.state NOT IN
                   ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed')
             UNION ALL
             SELECT i.content -> 'plan' -> 'baseline' ->> 'snapshot_id'
             FROM publication_intents i
             JOIN jobs j ON j.id = i.job_id
             WHERE j.organization_id = $1 AND j.table_identity_id = $2 AND j.id <> $3
               AND j.state NOT IN
                   ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed')
             UNION ALL
             SELECT i.content ->> 'baseline_snapshot_id'
             FROM publication_intents i
             JOIN jobs j ON j.id = i.job_id
             WHERE j.organization_id = $1 AND j.table_identity_id = $2 AND j.id <> $3
               AND j.operation = 'ExpireSnapshots'
               AND j.state NOT IN
                   ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed')
             UNION ALL
             SELECT element
             FROM publication_intents i
             JOIN jobs j ON j.id = i.job_id,
                  LATERAL jsonb_array_elements_text(
                      CASE WHEN jsonb_typeof(i.content -> 'protected') = 'array'
                           THEN i.content -> 'protected'
                           ELSE '[]'::jsonb
                      END
                  ) AS element
             WHERE j.organization_id = $1 AND j.table_identity_id = $2 AND j.id <> $3
               AND j.operation = 'ExpireSnapshots'
               AND j.state NOT IN
                   ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed')
         )
         SELECT DISTINCT snapshot::bigint
         FROM pinned
         WHERE snapshot ~ '^[0-9]+$' AND pg_input_is_valid(snapshot, 'bigint')
         ORDER BY 1",
    )
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .bind(exclude_job.as_uuid())
    .fetch_all(conn)
    .await?;
    Ok(snapshots.into_iter().collect())
}
