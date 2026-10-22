//! Worker identity binding, immutable polls, and database-time lease fencing.

use std::time::Duration;

use bergsmith_common::disposition::FailureDisposition;
use bergsmith_common::ids::{AttemptId, OrganizationId, PrincipalId, WorkerSessionId};
use bergsmith_common::job::JobState;
use bergsmith_common::task::{AttemptState, TaskState};
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::{PersistenceError, attempts, jobs, leadership, plans};

/// Identity established by the coordinator's token authenticator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// Tenant boundary.
    pub organization: OrganizationId,
    /// Authenticated service principal.
    pub principal: PrincipalId,
    /// Authorized pool.
    pub pool: String,
}

/// Validated registration parameters. One executing task per session.
#[derive(Clone, Debug)]
pub struct Registration {
    /// Fresh session identifier.
    pub id: WorkerSessionId,
    /// Diagnostic worker name.
    pub worker_name: String,
    /// Sorted, unique capabilities.
    pub capabilities: Vec<String>,
    /// Advertised memory capacity.
    pub memory_bytes: i64,
    /// Advertised CPU capacity.
    pub cpu_millis: i32,
    /// Diagnostic build version.
    pub build_version: String,
}

/// Durable session details, without credentials.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Session {
    /// Advertised capabilities.
    pub capabilities: Vec<String>,
    /// Advertised memory capacity.
    pub memory_bytes: i64,
    /// Advertised CPU capacity.
    pub cpu_millis: i32,
    /// Whether new grants are disabled.
    pub draining: bool,
    /// Session deadline in database time.
    pub lease_expires_at: DateTime<Utc>,
}

/// A current worker session as operators see it, without credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    /// Session identifier.
    pub id: WorkerSessionId,
    /// Service principal the session is bound to.
    pub principal_id: PrincipalId,
    /// Worker pool.
    pub pool: String,
    /// Diagnostic worker name.
    pub worker_name: String,
    /// Diagnostic build version.
    pub build_version: String,
    /// Advertised capabilities.
    pub capabilities: Vec<String>,
    /// Advertised memory capacity.
    pub memory_bytes: i64,
    /// Advertised CPU capacity.
    pub cpu_millis: i32,
    /// Lease state at database time.
    pub status: SessionStatus,
    /// When the session registered.
    pub registered_at: DateTime<Utc>,
    /// Session deadline in database time.
    pub lease_expires_at: DateTime<Utc>,
    /// The granted or running attempt, if any.
    pub running_attempt_id: Option<AttemptId>,
}

/// A session's lease state at database time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    /// Renewing its lease and eligible for grants.
    Live,
    /// Renewing its lease but receiving no new grants.
    Draining,
    /// Its lease expired; a restarted worker opens a new session.
    Expired,
}

impl SessionStatus {
    /// The stable name used by the API.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Draining => "draining",
            Self::Expired => "expired",
        }
    }
}

/// Lists an organization's current (not superseded) worker sessions in ID
/// order, starting after `after` and returning at most `limit` rows.
///
/// # Errors
/// Returns database errors.
pub async fn list_sessions(
    conn: &mut PgConnection,
    organization: OrganizationId,
    after: Option<WorkerSessionId>,
    limit: i64,
) -> Result<Vec<SessionSummary>, PersistenceError> {
    type Row = (
        Uuid,
        Uuid,
        String,
        String,
        String,
        Vec<String>,
        i64,
        i32,
        String,
        DateTime<Utc>,
        DateTime<Utc>,
        Option<Uuid>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT s.id, s.principal_id, s.pool, s.worker_name, s.build_version,
                s.capabilities, s.memory_bytes, s.cpu_millis,
                CASE WHEN s.lease_expires_at <= now() THEN 'expired'
                     WHEN s.draining THEN 'draining' ELSE 'live' END,
                s.created_at, s.lease_expires_at,
                (SELECT a.id FROM attempts a
                 WHERE a.worker_session_id = s.id AND a.state IN ('Granted', 'Running')
                 ORDER BY a.granted_at DESC LIMIT 1)
         FROM worker_sessions s
         WHERE s.organization_id = $1 AND NOT s.superseded
           AND ($2::uuid IS NULL OR s.id > $2)
         ORDER BY s.id LIMIT $3",
    )
    .bind(organization.as_uuid())
    .bind(after.map(|id| *id.as_uuid()))
    .bind(limit)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                principal_id,
                pool,
                worker_name,
                build_version,
                capabilities,
                memory_bytes,
                cpu_millis,
                status,
                registered_at,
                lease_expires_at,
                running,
            )| SessionSummary {
                id: WorkerSessionId::from_uuid(id),
                principal_id: PrincipalId::from_uuid(principal_id),
                pool,
                worker_name,
                build_version,
                capabilities,
                memory_bytes,
                cpu_millis,
                status: match status.as_str() {
                    "expired" => SessionStatus::Expired,
                    "draining" => SessionStatus::Draining,
                    _ => SessionStatus::Live,
                },
                registered_at,
                lease_expires_at,
                running_attempt_id: running.map(AttemptId::from_uuid),
            },
        )
        .collect())
}

fn invalid() -> PersistenceError {
    PersistenceError::InvalidValue {
        entity: "worker session",
        constraint: "worker_session_authority".into(),
    }
}

fn millis(duration: Duration) -> Result<i64, PersistenceError> {
    let value = i64::try_from(duration.as_millis()).map_err(|_| invalid())?;
    if value <= 0 {
        return Err(invalid());
    }
    Ok(value)
}

/// Opens a session bound to a service principal and tenant.
///
/// # Errors
/// Rejects non-service identities, malformed registration, or database failures.
pub async fn register(
    conn: &mut PgConnection,
    identity: &Identity,
    registration: &Registration,
    lease: Duration,
) -> Result<(), PersistenceError> {
    if registration.capabilities.windows(2).any(|w| w[0] >= w[1])
        || registration
            .capabilities
            .iter()
            .any(|c| c.is_empty() || c.len() > 200)
    {
        return Err(invalid());
    }
    let mut tx = conn.begin().await?;
    // Serialize registration retries for a worker name. A restart fences its
    // previous session; an old process may still write but cannot renew it.
    sqlx::query("SELECT id FROM principals WHERE id = $1 FOR UPDATE")
        .bind(identity.principal.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE worker_sessions SET superseded = true, draining = true,
        lease_expires_at = clock_timestamp() WHERE organization_id = $1
        AND principal_id = $2 AND pool = $3 AND worker_name = $4 AND NOT superseded",
    )
    .bind(identity.organization.as_uuid())
    .bind(identity.principal.as_uuid())
    .bind(&identity.pool)
    .bind(&registration.worker_name)
    .execute(&mut *tx)
    .await?;
    let inserted = sqlx::query(
        "INSERT INTO worker_sessions (id, organization_id, principal_id, pool, worker_name,
         capabilities, memory_bytes, cpu_millis, build_version, lease_expires_at)
         SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9,
                clock_timestamp() + $10 * interval '1 millisecond'
         FROM principals WHERE id = $3 AND organization_id = $2 AND kind = 'Service'",
    )
    .bind(registration.id.as_uuid())
    .bind(identity.organization.as_uuid())
    .bind(identity.principal.as_uuid())
    .bind(&identity.pool)
    .bind(&registration.worker_name)
    .bind(&registration.capabilities)
    .bind(registration.memory_bytes)
    .bind(registration.cpu_millis)
    .bind(&registration.build_version)
    .bind(millis(lease)?)
    .execute(&mut *tx)
    .await
    .map_err(|e| PersistenceError::from_write("worker session", e))?;
    if inserted.rows_affected() != 1 {
        return Err(invalid());
    }
    tx.commit().await?;
    Ok(())
}

/// Checks session ownership, including expired sessions used for acknowledgment recovery.
///
/// # Errors
/// Rejects unknown sessions and mismatched principals, tenants, or pools.
pub async fn session(
    conn: &mut PgConnection,
    identity: &Identity,
    id: WorkerSessionId,
) -> Result<Session, PersistenceError> {
    sqlx::query_as(
        "SELECT capabilities, memory_bytes, cpu_millis, draining, lease_expires_at
         FROM worker_sessions WHERE id = $1 AND organization_id = $2
         AND principal_id = $3 AND pool = $4",
    )
    .bind(id.as_uuid())
    .bind(identity.organization.as_uuid())
    .bind(identity.principal.as_uuid())
    .bind(&identity.pool)
    .fetch_optional(conn)
    .await?
    .ok_or_else(invalid)
}

async fn lock_session(
    conn: &mut PgConnection,
    identity: &Identity,
    id: WorkerSessionId,
) -> Result<Session, PersistenceError> {
    sqlx::query("SELECT id FROM worker_sessions WHERE id = $1 FOR UPDATE")
        .bind(id.as_uuid())
        .fetch_optional(&mut *conn)
        .await?;
    let session = session(conn, identity, id).await?;
    let live: bool = sqlx::query_scalar("SELECT $1::timestamptz > clock_timestamp()")
        .bind(session.lease_expires_at)
        .fetch_one(conn)
        .await?;
    if !live {
        return Err(invalid());
    }
    Ok(session)
}

/// A completed poll. `None` grant bytes means a durable no-task response.
///
/// This record never contains credentials.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Poll {
    /// Assignment, when one was made.
    pub attempt_id: Option<Uuid>,
    /// Canonical grant without any storage credentials.
    pub grant_bytes: Option<Vec<u8>>,
}

/// Checks session liveness by database wall time, locking it for the decision.
///
/// # Errors
/// Returns database errors for unknown sessions or unavailable storage.
pub async fn attempt_session_is_live(
    conn: &mut PgConnection,
    id: WorkerSessionId,
) -> Result<bool, PersistenceError> {
    let deadline: DateTime<Utc> =
        sqlx::query_scalar("SELECT lease_expires_at FROM worker_sessions WHERE id = $1 FOR SHARE")
            .bind(id.as_uuid())
            .fetch_one(&mut *conn)
            .await?;
    Ok(
        sqlx::query_scalar("SELECT $1::timestamptz > clock_timestamp()")
            .bind(deadline)
            .fetch_one(conn)
            .await?,
    )
}

/// Looks up a poll without selecting another task.
///
/// # Errors
/// Returns database errors; callers must authenticate the session first.
pub async fn poll(
    conn: &mut PgConnection,
    session: WorkerSessionId,
    request: Uuid,
) -> Result<Option<Poll>, PersistenceError> {
    Ok(sqlx::query_as(
        "SELECT attempt_id, grant_bytes FROM worker_polls
        WHERE worker_session_id = $1 AND request_id = $2",
    )
    .bind(session.as_uuid())
    .bind(request)
    .fetch_optional(conn)
    .await?)
}

/// Persists a no-task disposition, serialized with grants on the same session.
///
/// # Errors
/// Rejects expired or foreign sessions; database errors leave no completed poll.
pub async fn finish_empty_poll(
    conn: &mut PgConnection,
    identity: &Identity,
    id: WorkerSessionId,
    request: Uuid,
) -> Result<Poll, PersistenceError> {
    let mut tx = conn.begin().await?;
    lock_session(&mut tx, identity, id).await?;
    sqlx::query(
        "INSERT INTO worker_polls (worker_session_id, request_id)
        VALUES ($1, $2) ON CONFLICT (worker_session_id, request_id) DO NOTHING",
    )
    .bind(id.as_uuid())
    .bind(request)
    .execute(&mut *tx)
    .await?;
    let result = poll(&mut tx, id, request).await?.ok_or_else(invalid)?;
    tx.commit().await?;
    Ok(result)
}

/// Atomically creates an attempt and its poll association before a response.
/// Grant bytes must contain safe immutable fields only; credentials belong in memory.
///
/// # Errors
/// Rejects mismatched tenants, draining/expired sessions, busy workers, lost
/// leadership, invalid tasks, and duplicate active attempts. Exact polls replay.
/// Returns [`PersistenceError::StaleAttempt`] for `new.attempt_id`, persisting
/// nothing, when `payload.generation` is not the generation allocated under the
/// task lock; the caller may redraft the grant from a fresh task read.
pub async fn grant(
    conn: &mut PgConnection,
    identity: &Identity,
    new: &attempts::NewGrant,
    request: Uuid,
    epoch: i64,
    payload: &GrantPayload<'_>,
) -> Result<Poll, PersistenceError> {
    grant_inner(conn, identity, new, request, epoch, payload, None).await
}

/// Creates a managed grant with atomic table ownership and attempt-budget checks.
///
/// # Errors
/// Returns the errors of [`grant`] and `AdmissionUnavailable` when capacity is full.
pub async fn grant_admitted(
    conn: &mut PgConnection,
    identity: &Identity,
    new: &attempts::NewGrant,
    request: Uuid,
    epoch: i64,
    payload: &GrantPayload<'_>,
    limits: crate::scheduling::AdmissionLimits,
) -> Result<Poll, PersistenceError> {
    grant_inner(conn, identity, new, request, epoch, payload, Some(limits)).await
}

async fn grant_inner(
    conn: &mut PgConnection,
    identity: &Identity,
    new: &attempts::NewGrant,
    request: Uuid,
    epoch: i64,
    payload: &GrantPayload<'_>,
    limits: Option<crate::scheduling::AdmissionLimits>,
) -> Result<Poll, PersistenceError> {
    let mut tx = conn.begin().await?;
    let task = plans::task(&mut tx, new.task_id).await?;
    let job = jobs::lock_job(&mut tx, task.job_id).await?;
    let session = lock_session(&mut tx, identity, new.worker_session_id).await?;
    if let Some(previous) = poll(&mut tx, new.worker_session_id, request).await? {
        return Ok(previous);
    }
    if limits.is_none()
        && crate::scheduling::dispatch_spec(&mut tx, new.task_id)
            .await?
            .is_some()
    {
        return Err(invalid());
    }
    if let Some(limits) = limits
        && !crate::scheduling::authorize_grant(
            &mut tx,
            new.task_id,
            limits,
            &identity.pool,
            &session,
        )
        .await?
    {
        return Err(PersistenceError::AdmissionUnavailable);
    }
    if job.organization_id != identity.organization || session.draining {
        return Err(invalid());
    }
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM attempts
        WHERE worker_session_id = $1 AND state IN ('Granted', 'Running'))",
    )
    .bind(new.worker_session_id.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    if busy {
        return Err(invalid());
    }
    leadership::ensure_leadership(&mut tx, epoch).await?;
    bind_envelope(&mut tx, new.task_id, payload.envelope_bytes).await?;
    let attempt = attempts::grant_attempt(&mut tx, new).await?;
    // The caller encoded the grant from an unlocked task read. Dropping the
    // transaction rolls the attempt back when another attempt started meanwhile.
    if attempt.generation != payload.generation {
        return Err(PersistenceError::StaleAttempt {
            attempt: new.attempt_id,
        });
    }
    if job.state == JobState::Queued {
        jobs::transition_job(
            &mut tx,
            &jobs::Transition {
                job_id: job.id,
                expected_revision: job.state_revision,
                to: JobState::Running,
                reason: None,
                actor: None,
                non_publication_proven: false,
            },
        )
        .await?;
    }
    crate::publications::register_prefix(&mut tx, new.attempt_id, payload.output_prefix).await?;
    sqlx::query(
        "INSERT INTO worker_polls (worker_session_id, request_id, attempt_id, grant_bytes)
        VALUES ($1, $2, $3, $4)",
    )
    .bind(new.worker_session_id.as_uuid())
    .bind(request)
    .bind(new.attempt_id.as_uuid())
    .bind(payload.grant_bytes)
    .execute(&mut *tx)
    .await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    lock_session(&mut tx, identity, new.worker_session_id).await?;
    tx.commit().await?;
    poll(conn, new.worker_session_id, request)
        .await?
        .ok_or_else(invalid)
}

/// Safe grant fields and the original task resource envelope; never credentials.
pub struct GrantPayload<'a> {
    /// Encoded grant with its credentials omitted.
    pub grant_bytes: &'a [u8],
    /// Authorized typed-location prefix, already derived by the coordinator.
    pub output_prefix: &'a str,
    /// Canonical resource-limit message. It must match every prior generation.
    pub envelope_bytes: &'a [u8],
    /// Generation encoded in `grant_bytes`. It must equal the generation
    /// allocated under the task lock, or the grant is rejected as stale.
    pub generation: i32,
}

/// Binds a task's immutable resource envelope, including across coordinator restarts.
/// Reoffering the same bytes is harmless; changing them requires a new plan/job.
///
/// # Errors
/// Rejects mismatched envelopes and database failures without changing the original.
pub async fn bind_envelope(
    conn: &mut PgConnection,
    task: bergsmith_common::ids::TaskId,
    bytes: &[u8],
) -> Result<(), PersistenceError> {
    let mut tx = conn.begin().await?;
    sqlx::query(
        "INSERT INTO task_resource_envelopes (task_id, content) VALUES ($1, $2)
        ON CONFLICT (task_id) DO NOTHING",
    )
    .bind(task.as_uuid())
    .bind(bytes)
    .execute(&mut *tx)
    .await
    .map_err(|e| PersistenceError::from_write("task resource envelope", e))?;
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT content FROM task_resource_envelopes WHERE task_id = $1")
            .bind(task.as_uuid())
            .fetch_one(&mut *tx)
            .await?;
    if stored != bytes {
        return Err(PersistenceError::InvalidValue {
            entity: "task resource envelope",
            constraint: "task_resource_envelope_is_immutable".into(),
        });
    }
    tx.commit().await?;
    Ok(())
}

/// Checks that the authenticated session owns an attempt, even after expiry.
///
/// # Errors
/// Rejects foreign sessions or attempts without revealing their contents.
pub async fn owned_attempt(
    conn: &mut PgConnection,
    identity: &Identity,
    session_id: WorkerSessionId,
    attempt_id: AttemptId,
) -> Result<attempts::Attempt, PersistenceError> {
    session(conn, identity, session_id).await?;
    let attempt = attempts::attempt(conn, attempt_id).await?;
    if attempt.worker_session_id != session_id {
        return Err(invalid());
    }
    Ok(attempt)
}

/// Renews a live session without reviving an expired session.
///
/// # Errors
/// Rejects foreign or expired sessions and lost leadership.
pub async fn touch(
    conn: &mut PgConnection,
    identity: &Identity,
    id: WorkerSessionId,
    epoch: i64,
    lease: Duration,
) -> Result<Session, PersistenceError> {
    let mut tx = conn.begin().await?;
    lock_session(&mut tx, identity, id).await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    lock_session(&mut tx, identity, id).await?;
    sqlx::query(
        "UPDATE worker_sessions SET lease_expires_at =
        clock_timestamp() + $2 * interval '1 millisecond' WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(millis(lease)?)
    .execute(&mut *tx)
    .await?;
    let result = session(&mut tx, identity, id).await?;
    tx.commit().await?;
    Ok(result)
}

/// A heartbeat's authoritative instruction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Renewal {
    /// Keep executing until this database-time deadline.
    Renewed(DateTime<Utc>),
    /// Cancellation has removed publication authority.
    Cancel,
    /// This generation no longer owns the task.
    Fenced,
}

/// Renews an attempt only after all serialization locks and fresh clock checks.
///
/// # Errors
/// Rejects foreign attempts, expired sessions, invalid leases, and lost leadership.
pub async fn renew(
    conn: &mut PgConnection,
    identity: &Identity,
    id: WorkerSessionId,
    attempt_id: AttemptId,
    generation: i32,
    epoch: i64,
    lease: Duration,
) -> Result<Renewal, PersistenceError> {
    let mut tx = conn.begin().await?;
    let attempt = owned_attempt(&mut tx, identity, id, attempt_id).await?;
    let task = plans::task(&mut tx, attempt.task_id).await?;
    let job = jobs::lock_job(&mut tx, task.job_id).await?;
    lock_session(&mut tx, identity, id).await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    let (attempt, live) = attempts::lock_attempt(&mut tx, attempt_id).await?;
    if generation != attempt.generation || !live {
        return Ok(Renewal::Fenced);
    }
    if (matches!(job.state, JobState::Cancelling | JobState::Cancelled)
        || job.cancellation.is_some())
        && matches!(
            attempt.state,
            AttemptState::Granted | AttemptState::Running | AttemptState::Cancelled
        )
    {
        return Ok(Renewal::Cancel);
    }
    if attempt.state.is_terminal() || job.state != JobState::Running {
        return Ok(Renewal::Fenced);
    }
    let deadline = sqlx::query_scalar(
        "UPDATE attempts SET state = 'Running',
        lease_expires_at = clock_timestamp() + $2 * interval '1 millisecond'
        WHERE id = $1 RETURNING lease_expires_at",
    )
    .bind(attempt_id.as_uuid())
    .bind(millis(lease)?)
    .fetch_one(&mut *tx)
    .await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    lock_session(&mut tx, identity, id).await?;
    tx.commit().await?;
    Ok(Renewal::Renewed(deadline))
}

/// Drains a session; repeating it is harmless and existing work remains valid.
///
/// # Errors
/// Rejects foreign sessions and database failures.
pub async fn drain(
    conn: &mut PgConnection,
    identity: &Identity,
    id: WorkerSessionId,
) -> Result<(), PersistenceError> {
    session(conn, identity, id).await?;
    sqlx::query("UPDATE worker_sessions SET draining = true WHERE id = $1")
        .bind(id.as_uuid())
        .execute(conn)
        .await?;
    Ok(())
}

/// Records a worker failure once; worker-supplied messages/dispositions are untrusted.
/// Explicit transient native/storage reason codes may consume an execution retry.
/// BS-014 owns retry release, configurable backoff, and baseline revalidation.
///
/// # Errors
/// Rejects stale or foreign attempts, changed failure replays, and lost leadership.
pub async fn report_failure(
    conn: &mut PgConnection,
    identity: &Identity,
    session_id: WorkerSessionId,
    attempt_id: AttemptId,
    generation: i32,
    epoch: i64,
    reason: &str,
) -> Result<(FailureDisposition, JobState), PersistenceError> {
    if reason.is_empty() || reason.len() > 100 || !reason.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return Err(invalid());
    }
    let mut tx = conn.begin().await?;
    let attempt = owned_attempt(&mut tx, identity, session_id, attempt_id).await?;
    if generation != attempt.generation {
        return Err(PersistenceError::StaleAttempt {
            attempt: attempt_id,
        });
    }
    let task = plans::task(&mut tx, attempt.task_id).await?;
    let job = jobs::lock_job(&mut tx, task.job_id).await?;
    let previous: Option<(String, String, String)> = sqlx::query_as(
        "SELECT reason_code, disposition, job_state FROM attempt_failures WHERE attempt_id = $1",
    )
    .bind(attempt_id.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((stored, disposition, state)) = previous {
        if stored != reason {
            return Err(invalid());
        }
        let disposition = FailureDisposition::ALL
            .into_iter()
            .find(|d| d.as_str() == disposition)
            .ok_or_else(invalid)?;
        return Ok((
            disposition,
            JobState::from_name(&state).ok_or_else(invalid)?,
        ));
    }
    lock_session(&mut tx, identity, session_id).await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    let (attempt, live) = attempts::lock_attempt(&mut tx, attempt_id).await?;
    if !live
        || attempt.state.is_terminal()
        || job.state != JobState::Running
        || job.cancellation.is_some()
    {
        return Err(PersistenceError::StaleAttempt {
            attempt: attempt_id,
        });
    }
    let disposition = classify_failure(reason, generation);
    let retry = disposition == FailureDisposition::RetryTask;
    let retry_at = if retry {
        Some(crate::scheduling::retry_deadline(&mut tx, task.id, generation).await?)
    } else {
        None
    };
    attempts::end_attempt(
        &mut tx,
        &attempts::EndAttempt {
            attempt_id,
            generation,
            attempt_state: AttemptState::Failed,
            task_state: if retry {
                TaskState::RetryWaiting
            } else {
                TaskState::Failed
            },
            retry_not_before: retry_at,
        },
    )
    .await?;
    let mut state = job.state;
    if let Some(target) = disposition.target_job_state() {
        state = jobs::transition_job(
            &mut tx,
            &jobs::Transition {
                job_id: job.id,
                expected_revision: job.state_revision,
                to: target,
                reason: Some(reason.into()),
                actor: None,
                non_publication_proven: false,
            },
        )
        .await?
        .state;
    }
    sqlx::query(
        "INSERT INTO attempt_failures (attempt_id, reason_code, disposition, job_state)
        VALUES ($1, $2, $3, $4)",
    )
    .bind(attempt_id.as_uuid())
    .bind(reason)
    .bind(disposition.as_str())
    .bind(state.as_str())
    .execute(&mut *tx)
    .await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    lock_session(&mut tx, identity, session_id).await?;
    tx.commit().await?;
    Ok((disposition, state))
}

/// Classifies a worker-reported failure reason; the coordinator is authoritative.
///
/// Worker-local infrastructure faults (`WorkerLocalFailure`), an artifact read
/// that did not verify, and an unavailable development memory sample consume a
/// retry like a lost worker. A genuinely invalid grant, deterministic resource
/// limits, and unknown reasons are permanent.
fn classify_failure(reason: &str, generation: i32) -> FailureDisposition {
    match reason {
        "PlanningBaselineMismatch" => FailureDisposition::ReplanRequired,
        "StorageOperationTimedOut"
        | "PreparationTimeLimitExceeded"
        | "TransientNativeExecutionFailed"
        | "TransientMetadataReadFailed"
        | "TaskSubprocessCrashed"
        | "StorageAuthorizationExpired"
        | "WorkerExecutionInterrupted"
        | "WorkerLocalFailure"
        | "TaskArtifactUnverified"
        | "ProcessMemoryUsageUnavailable"
            if generation < 3 =>
        {
            FailureDisposition::RetryTask
        }
        _ => FailureDisposition::PermanentFailure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every reason a worker or its native executor can report, by disposition.
    const RETRYABLE: [&str; 10] = [
        "StorageOperationTimedOut",
        "PreparationTimeLimitExceeded",
        "TransientNativeExecutionFailed",
        "TransientMetadataReadFailed",
        "TaskSubprocessCrashed",
        "StorageAuthorizationExpired",
        "WorkerExecutionInterrupted",
        "WorkerLocalFailure",
        "TaskArtifactUnverified",
        "ProcessMemoryUsageUnavailable",
    ];
    const PERMANENT: [&str; 24] = [
        "InvalidTaskGrant",
        "TaskCancelled",
        "TaskDurationExceeded",
        "ProcessMemoryLimitExceeded",
        "ProcessCpuLimitExceeded",
        "BatchMemoryLimitExceeded",
        "DeleteMemoryLimitExceeded",
        "DecodeRowGroupLimitExceeded",
        "ReadBufferLimitExceeded",
        "FooterLimitExceeded",
        "OutputBytesLimitExceeded",
        "OutputFilesLimitExceeded",
        "InvalidExecutionLimits",
        "InvalidOutputPrefix",
        "ArtifactDigestMismatch",
        "InvalidTaskArtifact",
        "PlannedFileIdentityMismatch",
        "UnsupportedTaskInput",
        "InvalidTaskData",
        "MetadataVerificationFailed",
        "NativeExecutionFailed",
        "InvalidStorageLocation",
        "UnknownWorkerReason",
        "",
    ];

    #[test]
    fn worker_failure_classification_obeys_the_execution_budget_and_conflict_policy() {
        for generation in 1..=3 {
            assert_eq!(
                classify_failure("PlanningBaselineMismatch", generation),
                FailureDisposition::ReplanRequired
            );
            for reason in PERMANENT {
                assert_eq!(
                    classify_failure(reason, generation),
                    FailureDisposition::PermanentFailure,
                    "{reason}"
                );
            }
            for reason in RETRYABLE {
                assert_eq!(
                    classify_failure(reason, generation),
                    if generation < 3 {
                        FailureDisposition::RetryTask
                    } else {
                        FailureDisposition::PermanentFailure
                    },
                    "{reason}"
                );
            }
        }
    }

    #[test]
    fn worker_local_infrastructure_faults_retry_but_an_invalid_grant_is_permanent() {
        assert_eq!(
            classify_failure("WorkerLocalFailure", 1),
            FailureDisposition::RetryTask
        );
        assert_eq!(
            classify_failure("ProcessMemoryUsageUnavailable", 1),
            FailureDisposition::RetryTask
        );
        assert_eq!(
            classify_failure("InvalidTaskGrant", 1),
            FailureDisposition::PermanentFailure
        );
    }
}
