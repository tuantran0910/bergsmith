//! Cancellation requests and the send-authorization transaction.
//!
//! Both serialize on the job row lock. Whichever commits first decides the
//! outcome: a cancellation that wins prevents send authorization, and a send
//! authorization that wins makes later cancellation report
//! [`CancellationDisposition::TooLatePublicationStarted`].

use bergsmith_common::ids::{JobId, PrincipalId};
use bergsmith_common::job::{JobOperation, JobState};
use sqlx::{Connection, PgConnection};

use crate::PersistenceError;
use crate::jobs::{
    EventRecord, Job, JobEventKind, JobRow, Transition, append_event, job_columns, lock_job,
    transition_job,
};
use crate::leadership::ensure_leadership;

/// A request to cancel a job.
#[derive(Clone, Debug)]
pub struct CancelJob {
    /// The job to cancel.
    pub job_id: JobId,
    /// Who asks, if a principal does.
    pub requested_by: Option<PrincipalId>,
    /// Operator-supplied reason, if any.
    pub reason: Option<String>,
}

/// What a cancellation request achieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancellationDisposition {
    /// Cancellation won before send authorization. The job is `Cancelling`:
    /// grants, unaccepted results, and send authorization are fenced.
    Cancelling,
    /// A commit request was already authorized and may publish. The request
    /// is recorded and suppresses further replays, but cannot stop it.
    TooLatePublicationStarted,
    /// The commit outcome is unknown. The request is recorded for audit; no
    /// catalog abort is assumed.
    PublicationUnresolved,
    /// The job already published.
    TooLateCommitted,
    /// The job already ended without publishing, in the given state.
    AlreadyEnded(JobState),
}

/// The result of a cancellation request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancellationOutcome {
    /// What the request achieved.
    pub disposition: CancellationDisposition,
    /// The job after the request.
    pub job: Job,
}

/// A request to authorize the first outbound commit request.
#[derive(Clone, Copy, Debug)]
pub struct SendAuthorization {
    /// The job about to publish.
    pub job_id: JobId,
    /// The revision the publisher read; it must still be current.
    pub expected_revision: i64,
    /// The publishing coordinator's leadership epoch; it must still hold a
    /// live lease.
    pub coordinator_epoch: i64,
}

/// Records a cancellation request and applies it if it is not too late.
///
/// Requests are idempotent and monotonic: the first request's requester and
/// reason are kept, later requests change nothing and return the same
/// disposition. Before send authorization the job moves to `Cancelling` in
/// the same transaction. Terminal jobs are never modified.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the job does not exist, and
/// [`PersistenceError::Database`] when a query fails.
pub async fn request_cancellation(
    conn: &mut PgConnection,
    request: &CancelJob,
) -> Result<CancellationOutcome, PersistenceError> {
    let mut tx = conn.begin().await?;
    let job = lock_job(&mut tx, request.job_id).await?;
    let disposition = match job.state {
        // A cleanup publishes nothing, so its success is not a commit to be
        // too late for; it simply ended.
        JobState::Succeeded if job.operation == JobOperation::RemoveOrphanFiles => {
            CancellationDisposition::AlreadyEnded(job.state)
        }
        JobState::Succeeded => CancellationDisposition::TooLateCommitted,
        state if state.is_terminal() => CancellationDisposition::AlreadyEnded(state),
        JobState::CommitOutcomeUnknown => CancellationDisposition::PublicationUnresolved,
        _ if job.publication_started_at.is_some() => {
            CancellationDisposition::TooLatePublicationStarted
        }
        _ => CancellationDisposition::Cancelling,
    };
    if job.state.is_terminal() {
        return Ok(CancellationOutcome { disposition, job });
    }

    let job = if job.cancellation.is_none() {
        record_request(&mut tx, &job, request).await?
    } else {
        job
    };
    let job = if disposition == CancellationDisposition::Cancelling
        && job.state != JobState::Cancelling
    {
        transition_job(
            &mut tx,
            &Transition {
                job_id: job.id,
                expected_revision: job.state_revision,
                to: JobState::Cancelling,
                reason: Some("CancellationRequested".to_owned()),
                actor: request.requested_by,
                non_publication_proven: false,
            },
        )
        .await?
    } else {
        job
    };
    tx.commit().await?;
    Ok(CancellationOutcome { disposition, job })
}

async fn record_request(
    conn: &mut PgConnection,
    job: &Job,
    request: &CancelJob,
) -> Result<Job, PersistenceError> {
    let updated: Job = sqlx::query_as::<_, JobRow>(concat!(
        "UPDATE jobs SET cancellation_requested_at = clock_timestamp(), cancellation_requested_by = $2,
             cancellation_reason = $3, updated_at = clock_timestamp()
         WHERE id = $1
         RETURNING ",
        job_columns!()
    ))
    .bind(job.id.as_uuid())
    .bind(request.requested_by.as_ref().map(PrincipalId::as_uuid))
    .bind(&request.reason)
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write("cancellation request", error))?
    .try_into()?;
    append_event(
        conn,
        &EventRecord {
            job_id: updated.id,
            kind: JobEventKind::CancellationRequested,
            from_state: None,
            to_state: updated.state,
            state_revision: updated.state_revision,
            reason: request.reason.as_deref(),
            actor: request.requested_by,
        },
    )
    .await?;
    Ok(updated)
}

/// Durably authorizes the first outbound commit request for a job.
///
/// This is the short transaction that must commit immediately before the
/// request is sent, and no network call may happen inside it. It checks
/// leadership, the expected revision, that the job is `Committing`, that no cancellation was
/// requested, and that no send was authorized before, then records
/// `publication_started_at`. It does not change the job's state or revision.
///
/// # Errors
///
/// - [`PersistenceError::LeadershipLost`] when the coordinator's epoch no
///   longer holds a live leadership lease.
/// - [`PersistenceError::RevisionConflict`] when the job changed since the
///   publisher read it.
/// - [`PersistenceError::JobNotInState`] unless the job is `Committing`.
/// - [`PersistenceError::CancellationRequested`] when cancellation was
///   requested first.
/// - [`PersistenceError::PublicationAlreadyAuthorized`] when a send was
///   already authorized; replays are decided by publication recovery.
pub async fn authorize_send(
    conn: &mut PgConnection,
    authorization: SendAuthorization,
) -> Result<Job, PersistenceError> {
    let mut tx = conn.begin().await?;
    let job = lock_job(&mut tx, authorization.job_id).await?;
    ensure_leadership(&mut tx, authorization.coordinator_epoch).await?;
    if job.state_revision != authorization.expected_revision {
        return Err(PersistenceError::RevisionConflict {
            job: job.id,
            expected: authorization.expected_revision,
            actual: job.state_revision,
        });
    }
    if job.state != JobState::Committing {
        return Err(PersistenceError::JobNotInState {
            job: job.id,
            state: job.state,
            operation: "authorize a commit request",
        });
    }
    if job.cancellation.is_some() {
        return Err(PersistenceError::CancellationRequested { job: job.id });
    }
    if job.publication_started_at.is_some() {
        return Err(PersistenceError::PublicationAlreadyAuthorized { job: job.id });
    }
    // Recheck the requester's grant immediately before the one-shot send
    // authorization. Nothing has been sent yet, so a revoked grant prevents
    // publication without claiming a fact that is not yet known.
    crate::grants::require_job_permission(
        &mut tx,
        &job,
        bergsmith_common::permission::Permission::SubmitJobs,
    )
    .await?;
    // A policy disabled after admission stops the job before its one commit
    // request. Disabling only the schedule leaves an existing job alone.
    if crate::occurrences::job_policy_disabled(&mut tx, job.id).await? {
        return Err(PersistenceError::MaintenancePolicyDisabled { job: job.id });
    }

    let authorized: Job = sqlx::query_as::<_, JobRow>(concat!(
        "UPDATE jobs SET publication_started_at = clock_timestamp(), updated_at = clock_timestamp() WHERE id = $1
         RETURNING ",
        job_columns!()
    ))
    .bind(job.id.as_uuid())
    .fetch_one(&mut *tx)
    .await?
    .try_into()?;
    append_event(
        &mut tx,
        &EventRecord {
            job_id: authorized.id,
            kind: JobEventKind::PublicationAuthorized,
            from_state: None,
            to_state: authorized.state,
            state_revision: authorized.state_revision,
            reason: None,
            actor: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(authorized)
}
