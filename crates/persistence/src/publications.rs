//! Atomic publication intents, table barriers, and output inventories (BS-012).

use bergsmith_common::ids::{AttemptId, JobId, PublicationId};
use bergsmith_common::job::JobState;
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::PersistenceError;
use crate::attempts::{AcceptOutcome, Acceptance, ReceiptSubmission, accept_receipt};
use crate::jobs::{Job, Transition, lock_job, transition_job};
use crate::leadership::ensure_leadership;

/// Checks that the publication destination is an alias of this job's canonical table.
///
/// # Errors
/// Rejects another organization, catalog alias, or Iceberg table UUID.
pub async fn validate_destination(
    conn: &mut PgConnection,
    job: JobId,
    alias: &crate::tables::TableAlias,
    table_uuid: Uuid,
) -> Result<(), PersistenceError> {
    let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jobs j JOIN table_identities i ON i.id = j.table_identity_id JOIN table_aliases a ON a.table_identity_id = i.id AND a.organization_id = j.organization_id WHERE j.id = $1 AND a.catalog_connection_id = $2 AND a.namespace = $3 AND a.table_name = $4 AND i.table_uuid = $5)")
        .bind(job.as_uuid()).bind(alias.connection_id.as_uuid()).bind(&alias.namespace).bind(&alias.name).bind(table_uuid).fetch_one(conn).await?;
    if !valid {
        return Err(PersistenceError::InvalidValue {
            entity: "publication destination",
            constraint: "destination must match the job canonical table".into(),
        });
    }
    Ok(())
}

/// Lists jobs with durable publication work after a coordinator restart.
///
/// # Errors
/// Returns decoding/database errors without changing publication authority.
pub async fn recoverable_jobs(conn: &mut PgConnection) -> Result<Vec<Job>, PersistenceError> {
    use crate::jobs::{JobRow, job_columns};
    sqlx::query_as::<_, JobRow>(concat!(
        "SELECT ",
        job_columns!(),
        " FROM jobs WHERE state IN ('Committing', 'CommitOutcomeUnknown')
          OR (state = 'Cancelling' AND EXISTS (SELECT 1 FROM publication_intents i WHERE i.job_id = jobs.id)) ORDER BY id"
    ))
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(Job::try_from)
    .collect()
}

/// An immutable request and its recovery information. Content is coordinator-owned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    /// Job owning the accepted results.
    pub job_id: JobId,
    /// Single publication identity.
    pub publication_id: PublicationId,
    /// Baseline, receipt identities, destination, and expected snapshot.
    pub content: serde_json::Value,
    /// Exact serialized REST request, never re-encoded on recovery.
    pub request_body: Vec<u8>,
    /// Domain-separated request digest.
    pub request_hash: String,
}

/// An accepted receipt, in deterministic task ordinal order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedReceipt {
    /// Task's accepted attempt.
    pub attempt_id: AttemptId,
    /// Canonical encoded receipt.
    pub content: serde_json::Value,
    /// Canonical receipt digest.
    pub content_hash: String,
}

/// Reads immutable accepted receipts, ordered by task ordinal.
///
/// # Errors
/// Returns a database error if reading fails.
pub async fn accepted_receipts(
    conn: &mut PgConnection,
    job: JobId,
) -> Result<Vec<AcceptedReceipt>, PersistenceError> {
    let rows: Vec<(Uuid, serde_json::Value, String)> = sqlx::query_as(
        "SELECT r.attempt_id, r.content, r.content_hash FROM receipts r JOIN tasks t ON t.id = r.task_id WHERE t.job_id = $1 ORDER BY t.ordinal"
    ).bind(job.as_uuid()).fetch_all(conn).await?;
    Ok(rows
        .into_iter()
        .map(|(id, content, content_hash)| AcceptedReceipt {
            attempt_id: AttemptId::from_uuid(id),
            content,
            content_hash,
        })
        .collect())
}

/// Acquires the canonical table barrier, writes the intent, and enters `Committing` atomically.
///
/// The caller supplies exactly the receipts used to build the request. All tasks must
/// have accepted receipts. A second publication for an alias of the table is fenced.
///
/// # Errors
/// Returns revision, leadership, cancellation, receipt, or barrier conflicts without writes.
pub async fn create_intent(
    conn: &mut PgConnection,
    intent: &Intent,
    expected_revision: i64,
    epoch: i64,
    receipts: &[AcceptedReceipt],
) -> Result<Job, PersistenceError> {
    let mut tx = conn.begin().await?;
    let job = lock_job(&mut tx, intent.job_id).await?;
    ensure_leadership(&mut tx, epoch).await?;
    if job.state_revision != expected_revision {
        return Err(PersistenceError::RevisionConflict {
            job: job.id,
            expected: expected_revision,
            actual: job.state_revision,
        });
    }
    if job.cancellation.is_some() {
        return Err(PersistenceError::CancellationRequested { job: job.id });
    }
    if job.state != JobState::ReadyToCommit {
        return Err(PersistenceError::JobNotInState {
            job: job.id,
            state: job.state,
            operation: "create a publication intent",
        });
    }
    let incomplete: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks t LEFT JOIN receipts r ON r.task_id = t.id WHERE t.job_id = $1 AND (t.state <> 'Accepted' OR r.attempt_id IS NULL))")
        .bind(job.id.as_uuid()).fetch_one(&mut *tx).await?;
    if incomplete || accepted_receipts(&mut tx, job.id).await? != receipts {
        return Err(PersistenceError::InvalidValue {
            entity: "publication receipts",
            constraint: "all task receipts must match the intent".into(),
        });
    }
    sqlx::query("INSERT INTO publications (id, job_id) VALUES ($1, $2)")
        .bind(intent.publication_id.as_uuid())
        .bind(job.id.as_uuid())
        .execute(&mut *tx)
        .await
        .map_err(|e| PersistenceError::from_write("publication", e))?;
    sqlx::query("INSERT INTO publication_intents (job_id, publication_id, content, request_body, request_hash) VALUES ($1, $2, $3, $4, $5)")
        .bind(job.id.as_uuid()).bind(intent.publication_id.as_uuid()).bind(&intent.content).bind(&intent.request_body).bind(&intent.request_hash).execute(&mut *tx).await.map_err(|e| PersistenceError::from_write("publication intent", e))?;
    sqlx::query("INSERT INTO publication_barriers (table_identity_id, job_id) VALUES ($1, $2)")
        .bind(job.table_id.as_uuid())
        .bind(job.id.as_uuid())
        .execute(&mut *tx)
        .await
        .map_err(|e| PersistenceError::from_write("table publication barrier", e))?;
    let committing = transition_job(
        &mut tx,
        &Transition {
            job_id: job.id,
            expected_revision,
            to: JobState::Committing,
            reason: None,
            actor: None,
            non_publication_proven: false,
        },
    )
    .await?;
    // A competing publication can have held the unique table barrier long
    // enough for this lease to expire. Roll back the intent if leadership ended.
    ensure_leadership(&mut tx, epoch).await?;
    tx.commit().await?;
    Ok(committing)
}

/// Loads a durable intent without obtaining publication authority.
///
/// # Errors
/// Returns `NotFound` when the job has no intent.
pub async fn intent(conn: &mut PgConnection, job: JobId) -> Result<Intent, PersistenceError> {
    let row: Option<(Uuid, serde_json::Value, Vec<u8>, String)> = sqlx::query_as("SELECT publication_id, content, request_body, request_hash FROM publication_intents WHERE job_id = $1")
        .bind(job.as_uuid()).fetch_optional(conn).await?;
    let (id, content, request_body, request_hash) =
        row.ok_or_else(|| PersistenceError::NotFound {
            entity: "publication intent",
            id: job.to_string(),
        })?;
    Ok(Intent {
        job_id: job,
        publication_id: PublicationId::from_uuid(id),
        content,
        request_body,
        request_hash,
    })
}

/// Records the desired non-success state while an authorized publication is unresolved.
///
/// # Errors
/// Rejects nonterminal/success states and jobs without an unresolved intent.
pub async fn pend_disposition(
    conn: &mut PgConnection,
    job_id: JobId,
    state: JobState,
    reason: &str,
) -> Result<(), PersistenceError> {
    let mut tx = conn.begin().await?;
    let job = lock_job(&mut tx, job_id).await?;
    if !matches!(
        state,
        JobState::Failed | JobState::ReplanRequired | JobState::Cancelled
    ) || !matches!(
        job.state,
        JobState::Committing | JobState::CommitOutcomeUnknown
    ) {
        return Err(PersistenceError::JobNotInState {
            job: job.id,
            state: job.state,
            operation: "record a pending publication disposition",
        });
    }
    sqlx::query("INSERT INTO publication_pending_dispositions (job_id, state, reason) VALUES ($1, $2, $3) ON CONFLICT (job_id) DO NOTHING")
        .bind(job_id.as_uuid()).bind(state.as_str()).bind(reason).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Applies a confirmed outcome and releases the barrier in the same transaction.
///
/// `non_publication_proven` must cover every authorized sender, including delayed senders.
/// Unknown outcomes retain the barrier. Cancellation takes priority over pending failure.
///
/// # Errors
/// Returns revision/transition conflicts without releasing the barrier.
pub async fn resolve(
    conn: &mut PgConnection,
    job_id: JobId,
    revision: i64,
    to: JobState,
    reason: &str,
    non_publication_proven: bool,
) -> Result<Job, PersistenceError> {
    let mut tx = conn.begin().await?;
    let job = lock_job(&mut tx, job_id).await?;
    let pending: Option<(String, String)> = sqlx::query_as(
        "SELECT state, reason FROM publication_pending_dispositions WHERE job_id = $1",
    )
    .bind(job_id.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let (to, reason) = if to.is_terminal() && to != JobState::Succeeded {
        if job.cancellation.is_some() {
            (JobState::Cancelled, "CancellationRequested".to_owned())
        } else if let Some((state, reason)) = pending {
            (crate::jobs::parse_job_state(&state)?, reason)
        } else {
            (to, reason.to_owned())
        }
    } else {
        (to, reason.to_owned())
    };
    let updated = transition_job(
        &mut tx,
        &Transition {
            job_id,
            expected_revision: revision,
            to,
            reason: Some(reason),
            actor: None,
            non_publication_proven,
        },
    )
    .await?;
    // transition_job releases only this job's barrier on a terminal transition;
    // intent, receipts, and object inventory remain durable.
    tx.commit().await?;
    Ok(updated)
}

/// Registers a granted attempt's exact output prefix before execution.
///
/// # Errors
/// Rejects stale attempts, cancellation, and prefix reuse with different content.
pub async fn register_prefix(
    conn: &mut PgConnection,
    attempt: AttemptId,
    prefix: &str,
) -> Result<(), PersistenceError> {
    let mut tx = conn.begin().await?;
    let job_id: Uuid = sqlx::query_scalar(
        "SELECT t.job_id FROM attempts a JOIN tasks t ON t.id = a.task_id WHERE a.id = $1",
    )
    .bind(attempt.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    let job = lock_job(&mut tx, JobId::from_uuid(job_id)).await?;
    let (locked, live) = crate::attempts::lock_attempt(&mut tx, attempt).await?;
    if job.state != JobState::Running
        || job.cancellation.is_some()
        || locked.state.is_terminal()
        || !live
    {
        return Err(PersistenceError::StaleAttempt { attempt });
    }
    sqlx::query("INSERT INTO attempt_output_prefixes (attempt_id, prefix) VALUES ($1, $2) ON CONFLICT (attempt_id) DO NOTHING").bind(attempt.as_uuid()).bind(prefix).execute(&mut *tx).await.map_err(|e| PersistenceError::from_write("attempt output prefix", e))?;
    let stored: String =
        sqlx::query_scalar("SELECT prefix FROM attempt_output_prefixes WHERE attempt_id = $1")
            .bind(attempt.as_uuid())
            .fetch_one(&mut *tx)
            .await?;
    if stored != prefix {
        return Err(PersistenceError::InvalidValue {
            entity: "attempt output prefix",
            constraint: "prefix is immutable".into(),
        });
    }
    tx.commit().await?;
    Ok(())
}

/// Accepts verified receipt and finalized object inventory atomically against cancellation/expiry.
///
/// Objects are `(path, size)` in deterministic order. No storage I/O runs in this transaction.
///
/// # Errors
/// Returns receipt/prefix conflicts or database errors, with all writes rolled back.
pub async fn accept_outputs(
    conn: &mut PgConnection,
    submission: &ReceiptSubmission,
    prefix: &str,
    objects: &[(String, i64)],
) -> Result<Acceptance, PersistenceError> {
    let mut tx = conn.begin().await?;
    let mut accepted = accept_receipt(&mut tx, submission).await?;
    if accepted.outcome == AcceptOutcome::Accepted {
        let registered: Option<String> =
            sqlx::query_scalar("SELECT prefix FROM attempt_output_prefixes WHERE attempt_id = $1")
                .bind(submission.attempt_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
        if registered.as_deref() != Some(prefix) {
            return Err(PersistenceError::InvalidValue {
                entity: "attempt output prefix",
                constraint: "prefix must be registered before execution".into(),
            });
        }
        for (path, size) in objects {
            sqlx::query("INSERT INTO attempt_output_objects (attempt_id, path, size_bytes) VALUES ($1, $2, $3)").bind(submission.attempt_id.as_uuid()).bind(path).bind(size).execute(&mut *tx).await.map_err(|e| PersistenceError::from_write("attempt output object", e))?;
        }
        let job_id: Uuid = sqlx::query_scalar(
            "SELECT t.job_id FROM attempts a JOIN tasks t ON t.id = a.task_id WHERE a.id = $1",
        )
        .bind(submission.attempt_id.as_uuid())
        .fetch_one(&mut *tx)
        .await?;
        let incomplete: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE job_id = $1 AND state <> 'Accepted')",
        )
        .bind(job_id)
        .fetch_one(&mut *tx)
        .await?;
        if !incomplete {
            let job = lock_job(&mut tx, JobId::from_uuid(job_id)).await?;
            let ready = transition_job(
                &mut tx,
                &Transition {
                    job_id: job.id,
                    expected_revision: job.state_revision,
                    to: JobState::ReadyToCommit,
                    reason: None,
                    actor: None,
                    non_publication_proven: false,
                },
            )
            .await?;
            accepted.job_state = ready.state;
        }
    }
    tx.commit().await?;
    Ok(accepted)
}

/// Retained output prefix and finalized objects, with a state derived from the job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputInventory {
    /// Attempt whose objects must remain accounted for.
    pub attempt_id: AttemptId,
    /// Registered before execution, including failed/incomplete writes.
    pub prefix: String,
    /// Finalized objects accepted with the receipt, ordered by path.
    pub objects: Vec<(String, i64)>,
    /// Why these objects are retained. No state authorizes automatic deletion.
    pub disposition: OutputDisposition,
}

/// Lifecycle classification for operator inventory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputDisposition {
    /// Prefix may contain active or incomplete writes.
    Executing,
    /// Accepted output awaiting a publication decision.
    AwaitingPublication,
    /// A publication may still reference these objects; retain them indefinitely.
    PublicationUnresolved,
    /// Confirmed publication references the accepted output.
    Published,
    /// Failed, cancelled, expired, or superseded attempt output; cleanup is separate.
    Abandoned,
}

/// Lists every registered attempt prefix for a job, including unsuccessful attempts.
///
/// # Errors
/// Returns database errors without changing objects or retention state.
pub async fn output_inventory(
    conn: &mut PgConnection,
    job: JobId,
) -> Result<Vec<OutputInventory>, PersistenceError> {
    let rows: Vec<(Uuid, String, String, String, bool, bool)> = sqlx::query_as(
        "SELECT a.id, p.prefix, j.state, a.state, r.attempt_id IS NOT NULL, j.publication_started_at IS NOT NULL FROM attempt_output_prefixes p JOIN attempts a ON a.id = p.attempt_id JOIN tasks t ON t.id = a.task_id JOIN jobs j ON j.id = t.job_id LEFT JOIN receipts r ON r.attempt_id = a.id WHERE j.id = $1 ORDER BY a.id"
    ).bind(job.as_uuid()).fetch_all(&mut *conn).await?;
    let mut inventory = Vec::new();
    for (id, prefix, state, attempt_state, accepted, authorized) in rows {
        let state = crate::jobs::parse_job_state(&state)?;
        let disposition = if accepted && state == JobState::Succeeded {
            OutputDisposition::Published
        } else if accepted
            && (state == JobState::CommitOutcomeUnknown
                || (state == JobState::Committing && authorized))
        {
            OutputDisposition::PublicationUnresolved
        } else if state.is_terminal()
            || state == JobState::Cancelling
            || matches!(attempt_state.as_str(), "Expired" | "Failed" | "Cancelled")
        {
            OutputDisposition::Abandoned
        } else if accepted {
            OutputDisposition::AwaitingPublication
        } else {
            OutputDisposition::Executing
        };
        let objects = sqlx::query_as("SELECT path, size_bytes FROM attempt_output_objects WHERE attempt_id = $1 ORDER BY path")
            .bind(id).fetch_all(&mut *conn).await?;
        inventory.push(OutputInventory {
            attempt_id: AttemptId::from_uuid(id),
            prefix,
            objects,
            disposition,
        });
    }
    Ok(inventory)
}
