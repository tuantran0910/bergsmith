//! Durable dispatch configuration, atomic admission budgets, and execution repair.

use bergsmith_common::ids::{JobId, TaskId};
use bergsmith_common::job::JobState;
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::{PersistenceError, attempts, jobs, leadership, workers};

/// Configurable admission limits. Reservations remain charged through publication.
#[derive(Clone, Copy, Debug)]
pub struct AdmissionLimits {
    /// Maximum admitted nonterminal jobs in the coordinator.
    pub jobs: i64,
    /// Maximum admitted jobs in one organization.
    pub organization_jobs: i64,
    /// Maximum admitted jobs in one worker pool.
    pub pool_jobs: i64,
    /// Maximum active attempts across managed jobs.
    pub attempts: i64,
    /// Total managed active-attempt memory reservation.
    pub memory_bytes: i64,
    /// Total managed active-attempt CPU reservation.
    pub cpu_millis: i64,
}

impl Default for AdmissionLimits {
    fn default() -> Self {
        Self {
            jobs: 8,
            organization_jobs: 4,
            pool_jobs: 8,
            attempts: 16,
            memory_bytes: 64 * 1024 * 1024 * 1024,
            cpu_millis: 32_000,
        }
    }
}

impl AdmissionLimits {
    /// Whether every budget is positive.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.jobs > 0
            && self.organization_jobs > 0
            && self.pool_jobs > 0
            && self.attempts > 0
            && self.memory_bytes > 0
            && self.cpu_millis > 0
    }
}

/// Immutable per-job bounded exponential backoff policy, in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Initial backoff ceiling, before deterministic equal jitter.
    pub base_millis: i64,
    /// Maximum backoff ceiling.
    pub max_millis: i64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base_millis: 1_000,
            max_millis: 60_000,
        }
    }
}

impl RetryPolicy {
    /// Whether bounds are positive, ordered, and representable by PostgreSQL.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.base_millis > 0 && self.max_millis >= self.base_millis && self.max_millis <= 86_400_000
    }

    /// Equal jitter in the upper half of the capped exponential window.
    /// Stable task/generation entropy makes the policy reproducible after restart.
    #[must_use]
    fn delay_millis(self, task: TaskId, generation: i32) -> i64 {
        let shift = u32::try_from(generation.saturating_sub(1))
            .unwrap_or(0)
            .min(2);
        let ceiling = self
            .base_millis
            .saturating_mul(1_i64 << shift)
            .min(self.max_millis);
        let floor = (ceiling / 2).max(1);
        let width = u128::try_from(ceiling - floor + 1).unwrap_or(1);
        floor + i64::try_from((task.as_uuid().as_u128() ^ u128::from(shift)) % width).unwrap_or(0)
    }
}

/// Safe task dispatch fields reconstructed on coordinator restart.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct DispatchSpec {
    /// Immutable task identifier.
    pub task_id: Uuid,
    /// Worker pool.
    pub pool: String,
    /// Required worker capability.
    pub capability: String,
    /// Fetchable artifact URI.
    pub artifact_uri: String,
    /// Canonical artifact digest.
    pub artifact_sha256: Vec<u8>,
    /// Canonical protobuf resource limits, without credentials.
    pub envelope_bytes: Vec<u8>,
    /// Memory reserved per active attempt.
    pub memory_bytes: i64,
    /// CPU reserved per active attempt.
    pub cpu_millis: i32,
}

fn invalid(constraint: &str) -> PersistenceError {
    PersistenceError::InvalidValue {
        entity: "scheduler",
        constraint: constraint.into(),
    }
}

async fn guard(conn: &mut PgConnection) -> Result<(), PersistenceError> {
    sqlx::query("SELECT singleton FROM scheduler_guard FOR UPDATE")
        .execute(conn)
        .await?;
    Ok(())
}

/// Registers dispatch only while `epoch` holds live leadership.
///
/// # Errors
/// Returns lost leadership or the validation/database errors of [`register_dispatch`].
pub async fn register_dispatch_at_epoch(
    conn: &mut PgConnection,
    job: JobId,
    specs: &[DispatchSpec],
    retry: RetryPolicy,
    epoch: i64,
) -> Result<(), PersistenceError> {
    let mut tx = conn.begin().await?;
    jobs::lock_job(&mut tx, job).await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    register_dispatch(&mut tx, job, specs, retry).await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    tx.commit().await?;
    Ok(())
}

/// Persists all of a job's dispatch fields atomically and immutably.
///
/// # Errors
/// Rejects incomplete plans, mixed pools, terminal jobs, and changed replays.
pub async fn register_dispatch(
    conn: &mut PgConnection,
    job: JobId,
    specs: &[DispatchSpec],
    retry: RetryPolicy,
) -> Result<(), PersistenceError> {
    if specs.is_empty() || !retry.is_valid() || specs.iter().any(|s| s.pool != specs[0].pool) {
        return Err(invalid("invalid_dispatch_configuration"));
    }
    let mut tx = conn.begin().await?;
    let locked = jobs::lock_job(&mut tx, job).await?;
    if let Some(previous) = dispatch_registration(&mut tx, job).await? {
        return if previous.matches(specs, retry) {
            Ok(())
        } else {
            Err(invalid("dispatch_registration_is_immutable"))
        };
    }
    if !matches!(locked.state, JobState::Queued | JobState::Running)
        || locked.cancellation.is_some()
    {
        return Err(invalid("job_is_not_dispatchable"));
    }
    let tasks: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM tasks WHERE job_id = $1 ORDER BY id")
        .bind(job.as_uuid())
        .fetch_all(&mut *tx)
        .await?;
    let mut supplied: Vec<_> = specs.iter().map(|s| s.task_id).collect();
    supplied.sort_unstable();
    if tasks != supplied {
        return Err(invalid("dispatch_must_cover_exactly_the_plan"));
    }
    sqlx::query("INSERT INTO job_retry_policies VALUES ($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(job.as_uuid())
        .bind(retry.base_millis)
        .bind(retry.max_millis)
        .execute(&mut *tx)
        .await?;
    let stored: (i64, i64) =
        sqlx::query_as("SELECT base_millis, max_millis FROM job_retry_policies WHERE job_id=$1")
            .bind(job.as_uuid())
            .fetch_one(&mut *tx)
            .await?;
    if stored != (retry.base_millis, retry.max_millis) {
        return Err(invalid("retry_policy_is_immutable"));
    }
    for spec in specs {
        workers::bind_envelope(
            &mut tx,
            TaskId::from_uuid(spec.task_id),
            &spec.envelope_bytes,
        )
        .await?;
        sqlx::query(
            "INSERT INTO dispatch_specs VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
        )
        .bind(spec.task_id)
        .bind(&spec.pool)
        .bind(&spec.capability)
        .bind(&spec.artifact_uri)
        .bind(&spec.artifact_sha256)
        .bind(&spec.envelope_bytes)
        .bind(spec.memory_bytes)
        .bind(spec.cpu_millis)
        .execute(&mut *tx)
        .await?;
        if dispatch_spec(&mut tx, TaskId::from_uuid(spec.task_id))
            .await?
            .as_ref()
            != Some(spec)
        {
            return Err(invalid("dispatch_spec_is_immutable"));
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Reads the immutable configuration, if the task is managed by the scheduler.
///
/// # Errors
/// Returns database errors without changing scheduling state.
pub async fn dispatch_spec(
    conn: &mut PgConnection,
    task: TaskId,
) -> Result<Option<DispatchSpec>, PersistenceError> {
    Ok(
        sqlx::query_as("SELECT * FROM dispatch_specs WHERE task_id=$1")
            .bind(task.as_uuid())
            .fetch_optional(conn)
            .await?,
    )
}

/// An atomically registered immutable plan's dispatch configuration.
pub struct DispatchRegistration {
    /// Specifications in task-ID order.
    pub specs: Vec<DispatchSpec>,
    /// Original per-job retry bounds.
    pub retry: RetryPolicy,
}

impl DispatchRegistration {
    /// Whether a replay is identical, independent of request ordering.
    #[must_use]
    pub fn matches(&self, specs: &[DispatchSpec], retry: RetryPolicy) -> bool {
        let mut supplied: Vec<_> = specs.iter().collect();
        supplied.sort_unstable_by_key(|s| s.task_id);
        self.retry == retry && self.specs.iter().eq(supplied)
    }
}

/// Reads a completed registration without catalog/artifact access, even after termination.
///
/// # Errors
/// Returns database errors; partial configurations cannot be committed by registration.
pub async fn dispatch_registration(
    conn: &mut PgConnection,
    job: JobId,
) -> Result<Option<DispatchRegistration>, PersistenceError> {
    let policy: Option<(i64, i64)> =
        sqlx::query_as("SELECT base_millis,max_millis FROM job_retry_policies WHERE job_id=$1")
            .bind(job.as_uuid())
            .fetch_optional(&mut *conn)
            .await?;
    let Some((base_millis, max_millis)) = policy else {
        return Ok(None);
    };
    let specs = sqlx::query_as("SELECT d.* FROM dispatch_specs d JOIN tasks t ON t.id=d.task_id WHERE t.job_id=$1 ORDER BY d.task_id")
        .bind(job.as_uuid()).fetch_all(conn).await?;
    Ok(Some(DispatchRegistration {
        specs,
        retry: RetryPolicy {
            base_millis,
            max_millis,
        },
    }))
}

/// Acquires table ownership and job slots, or returns false while budgets are full.
///
/// # Errors
/// Rejects stale leadership or non-dispatchable jobs; aliases share the canonical table.
pub async fn admit(
    conn: &mut PgConnection,
    job_id: JobId,
    epoch: i64,
    limits: AdmissionLimits,
) -> Result<bool, PersistenceError> {
    let mut tx = conn.begin().await?;
    let job = jobs::lock_job(&mut tx, job_id).await?;
    // Revocation takes effect at the next admission, inside the same
    // transaction that would reserve budgets and open the table.
    crate::grants::require_job_permission(
        &mut tx,
        &job,
        bergsmith_common::permission::Permission::SubmitJobs,
    )
    .await?;
    if !limits.is_valid()
        || !matches!(job.state, JobState::Queued | JobState::Running)
        || job.cancellation.is_some()
    {
        return Err(invalid("job_is_not_admissible"));
    }
    guard(&mut tx).await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    if is_admitted(&mut tx, job_id).await? {
        return Ok(true);
    }
    if !crate::tenancy::lock_execution_route(
        &mut tx,
        job.organization_id,
        job.created_by,
        job.table_id,
    )
    .await?
    .is_allowed()
    {
        return Ok(false);
    }
    let Some(pool) = admission_open(&mut tx, &job, limits).await? else {
        return Ok(false);
    };
    sqlx::query(
        "INSERT INTO execution_admissions (job_id,table_identity_id,pool) VALUES ($1,$2,$3)",
    )
    .bind(job_id.as_uuid())
    .bind(job.table_id.as_uuid())
    .bind(pool)
    .execute(&mut *tx)
    .await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    tx.commit().await?;
    Ok(true)
}

async fn is_admitted(conn: &mut PgConnection, job_id: JobId) -> Result<bool, PersistenceError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM execution_admissions WHERE job_id=$1 AND NOT released)",
    )
    .bind(job_id.as_uuid())
    .fetch_one(conn)
    .await?)
}

/// The job's pool when its table and every job budget have room, else `None`.
async fn admission_open(
    conn: &mut PgConnection,
    job: &jobs::Job,
    limits: AdmissionLimits,
) -> Result<Option<String>, PersistenceError> {
    let pool: String = sqlx::query_scalar("SELECT d.pool FROM dispatch_specs d JOIN tasks t ON t.id=d.task_id WHERE t.job_id=$1 LIMIT 1")
        .bind(job.id.as_uuid()).fetch_optional(&mut *conn).await?.ok_or_else(|| invalid("missing_dispatch_configuration"))?;
    let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM execution_admissions WHERE table_identity_id=$1 AND NOT released) OR EXISTS(SELECT 1 FROM publication_barriers WHERE table_identity_id=$1 AND job_id<>$2) OR EXISTS(SELECT 1 FROM jobs WHERE table_identity_id=$1 AND id<>$2 AND state IN ('Running','ReadyToCommit','Committing','CommitOutcomeUnknown','Cancelling'))")
        .bind(job.table_id.as_uuid()).bind(job.id.as_uuid()).fetch_one(&mut *conn).await?;
    let counts: (i64,i64,i64) = sqlx::query_as("SELECT count(*), count(*) FILTER (WHERE j.organization_id=$1), count(*) FILTER (WHERE a.pool=$2) FROM execution_admissions a JOIN jobs j ON j.id=a.job_id WHERE NOT a.released")
        .bind(job.organization_id.as_uuid()).bind(&pool).fetch_one(&mut *conn).await?;
    Ok((!blocked
        && counts.0 < limits.jobs
        && counts.1 < limits.organization_jobs
        && counts.2 < limits.pool_jobs)
        .then_some(pool))
}

/// What a nonterminal job's next reconciliation step needs from external validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationNeed {
    /// Nothing would be admitted or released; skip catalog/storage validation.
    None,
    /// The job is unadmitted and its table and budgets currently have room.
    Admission,
    /// The admitted job has retries whose durable deadline has passed.
    RetryRelease,
}

/// Decides, without writes, whether reconciliation must validate `job`, a
/// recently read snapshot. Brief shared connection locks are used to check
/// enabled aliases; outside a transaction those locks end with that statement,
/// and the subsequent grant check is only an admission hint.
///
/// Validation precedes every admission and retry release; an admitted job with
/// no due retries keeps its earlier validation. A job blocked by its table or
/// budgets is not validated until it could actually be admitted. [`admit`] and
/// [`reconcile_execution`] recheck everything inside their transactions.
///
/// # Errors
/// Returns database errors or a missing dispatch configuration.
pub async fn validation_need(
    conn: &mut PgConnection,
    job: &jobs::Job,
    limits: AdmissionLimits,
) -> Result<ValidationNeed, PersistenceError> {
    let job_id = job.id;
    if !matches!(job.state, JobState::Queued | JobState::Running) || job.cancellation.is_some() {
        return Ok(ValidationNeed::None);
    }
    if !is_admitted(conn, job_id).await? {
        if !crate::tenancy::lock_execution_route(
            conn,
            job.organization_id,
            job.created_by,
            job.table_id,
        )
        .await?
        .is_allowed()
        {
            return Ok(ValidationNeed::None);
        }
        return Ok(if admission_open(conn, job, limits).await?.is_some() {
            ValidationNeed::Admission
        } else {
            ValidationNeed::None
        });
    }
    let due: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE job_id=$1 AND state='RetryWaiting' AND retry_not_before<=clock_timestamp())")
        .bind(job_id.as_uuid()).fetch_one(conn).await?;
    Ok(if due {
        ValidationNeed::RetryRelease
    } else {
        ValidationNeed::None
    })
}

/// Serializes publication-barrier creation with execution-table admission.
/// The caller holds its job lock and must acquire leadership after this guard.
pub(crate) async fn authorize_publication(
    conn: &mut PgConnection,
    job: &jobs::Job,
) -> Result<(), PersistenceError> {
    // The requester's grant is rechecked before intent construction and again
    // before send authorization. Revocation therefore prevents a new
    // publication without touching worker credentials.
    crate::grants::require_job_permission(
        conn,
        job,
        bergsmith_common::permission::Permission::SubmitJobs,
    )
    .await?;
    // A scheduled job also needs its policy to be enabled at publication. A
    // disabled schedule does not block an existing job; disabling it only
    // stops future admission.
    if crate::occurrences::job_policy_disabled(conn, job.id).await? {
        return Err(PersistenceError::MaintenancePolicyDisabled { job: job.id });
    }
    guard(conn).await?;
    let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM execution_admissions WHERE table_identity_id=$1 AND job_id<>$2 AND NOT released)")
        .bind(job.table_id.as_uuid()).bind(job.id.as_uuid()).fetch_one(conn).await?;
    if blocked {
        return Err(PersistenceError::AdmissionUnavailable);
    }
    Ok(())
}

/// Checks attempt budgets inside the grant transaction, after its job lock.
pub(crate) async fn authorize_grant(
    conn: &mut PgConnection,
    task: TaskId,
    limits: AdmissionLimits,
    pool: &str,
    session: &workers::Session,
) -> Result<bool, PersistenceError> {
    guard(conn).await?;
    let spec = dispatch_spec(conn, task)
        .await?
        .ok_or_else(|| invalid("missing_dispatch_configuration"))?;
    if spec.pool != pool
        || !session.capabilities.contains(&spec.capability)
        || spec.memory_bytes > session.memory_bytes
        || spec.cpu_millis > session.cpu_millis
    {
        return Err(invalid("worker_is_not_eligible_for_dispatch"));
    }
    let admitted: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks t JOIN execution_admissions a ON a.job_id=t.job_id WHERE t.id=$1 AND NOT a.released)")
        .bind(task.as_uuid()).fetch_one(&mut *conn).await?;
    let used: (i64,i64,i64) = sqlx::query_as("SELECT count(*), COALESCE(sum(d.memory_bytes),0)::bigint, COALESCE(sum(d.cpu_millis),0)::bigint FROM attempts a JOIN dispatch_specs d ON d.task_id=a.task_id WHERE a.state IN ('Granted','Running')")
        .fetch_one(conn).await?;
    Ok(admitted
        && used.0 < limits.attempts
        && used.1 <= limits.memory_bytes.saturating_sub(spec.memory_bytes)
        && used.2 <= limits.cpu_millis.saturating_sub(i64::from(spec.cpu_millis)))
}

/// A bounded, ordered page of nonterminal jobs for startup and periodic repair.
///
/// # Errors
/// Returns database or corrupt-row errors.
pub async fn active_jobs(
    conn: &mut PgConnection,
    after: Uuid,
    limit: i64,
) -> Result<Vec<jobs::Job>, PersistenceError> {
    let rows = sqlx::query_as::<_, jobs::JobRow>(concat!("SELECT ", jobs::job_columns!(), " FROM jobs WHERE id>$1 AND state NOT IN ('Succeeded','NoOp','Cancelled','ReplanRequired','Failed') ORDER BY id LIMIT $2"))
        .bind(after).bind(limit).fetch_all(conn).await?;
    rows.into_iter().map(jobs::Job::try_from).collect()
}

/// Durable ready membership, ordered independently of scan/notification order.
///
/// # Errors
/// Returns database errors.
pub async fn ready(
    conn: &mut PgConnection,
    limit: i64,
) -> Result<Vec<(Uuid, Uuid)>, PersistenceError> {
    ready_after(conn, Uuid::nil(), limit).await
}

/// Reads a rotating bounded window, wrapping after the final identifier.
/// Rotation prevents one pool's unclaimed tasks monopolizing the ready cache.
///
/// # Errors
/// Returns database errors.
pub async fn ready_after(
    conn: &mut PgConnection,
    after: Uuid,
    limit: i64,
) -> Result<Vec<(Uuid, Uuid)>, PersistenceError> {
    Ok(sqlx::query_as(
        "SELECT task_id, organization_id FROM (
        (SELECT t.id AS task_id,j.organization_id,0 AS wrap FROM tasks t
         JOIN jobs j ON j.id=t.job_id JOIN execution_admissions a ON a.job_id=j.id
         WHERE NOT a.released AND j.state IN ('Queued','Running')
         AND j.cancellation_requested_at IS NULL AND t.state='Pending' AND t.id>$2
         ORDER BY t.id LIMIT $1)
        UNION ALL
        (SELECT t.id AS task_id,j.organization_id,1 AS wrap FROM tasks t
         JOIN jobs j ON j.id=t.job_id JOIN execution_admissions a ON a.job_id=j.id
         WHERE NOT a.released AND j.state IN ('Queued','Running')
         AND j.cancellation_requested_at IS NULL AND t.state='Pending' AND t.id<=$2
         ORDER BY t.id LIMIT $1)
        ) candidates ORDER BY wrap,task_id LIMIT $1",
    )
    .bind(limit)
    .bind(after)
    .fetch_all(conn)
    .await?)
}

/// Returns persisted retry timers, including timers not due yet.
///
/// Covers exactly the jobs whose retries [`reconcile_execution`] may release:
/// dispatchable (`Queued` or `Running`) jobs without a cancellation request.
///
/// # Errors
/// Returns database errors.
pub async fn retry_timers(
    conn: &mut PgConnection,
) -> Result<Vec<(Uuid, DateTime<Utc>)>, PersistenceError> {
    Ok(sqlx::query_as("SELECT t.id,t.retry_not_before FROM tasks t JOIN jobs j ON j.id=t.job_id WHERE t.state='RetryWaiting' AND j.state IN ('Queued','Running') AND j.cancellation_requested_at IS NULL ORDER BY t.retry_not_before,t.id").fetch_all(conn).await?)
}

/// Chooses a retry deadline once, using a job's immutable policy and database time.
pub(crate) async fn retry_deadline(
    conn: &mut PgConnection,
    task: TaskId,
    generation: i32,
) -> Result<DateTime<Utc>, PersistenceError> {
    let policy: Option<(i64,i64)> = sqlx::query_as("SELECT p.base_millis,p.max_millis FROM job_retry_policies p JOIN tasks t ON t.job_id=p.job_id WHERE t.id=$1")
        .bind(task.as_uuid()).fetch_optional(&mut *conn).await?;
    let delay = policy.map_or(1_000, |(base_millis, max_millis)| {
        RetryPolicy {
            base_millis,
            max_millis,
        }
        .delay_millis(task, generation)
    });
    Ok(
        sqlx::query_scalar("SELECT clock_timestamp() + $1 * interval '1 millisecond'")
            .bind(delay)
            .fetch_one(conn)
            .await?,
    )
}

/// Repairs expired/session-fenced attempts and releases due retries for a validated job.
/// Call only after checking the original baseline, retention, and current authority.
///
/// # Errors
/// Rejects lost leadership. Catalog errors must never call this method.
pub async fn reconcile_execution(
    conn: &mut PgConnection,
    job_id: JobId,
    epoch: i64,
) -> Result<(), PersistenceError> {
    repair(conn, job_id, epoch, true).await
}

/// Fences expired or session-lost attempts and schedules their durable retry
/// deadlines, without releasing any retry. An exhausted budget fails the job.
///
/// This needs no external validation: it only ends execution that can no
/// longer be authoritative. Releasing a due retry still requires
/// [`reconcile_execution`] after validation.
///
/// # Errors
/// Rejects lost leadership.
pub async fn repair_attempts(
    conn: &mut PgConnection,
    job_id: JobId,
    epoch: i64,
) -> Result<(), PersistenceError> {
    repair(conn, job_id, epoch, false).await
}

async fn repair(
    conn: &mut PgConnection,
    job_id: JobId,
    epoch: i64,
    release: bool,
) -> Result<(), PersistenceError> {
    let mut tx = conn.begin().await?;
    let job = jobs::lock_job(&mut tx, job_id).await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    if !matches!(job.state, JobState::Queued | JobState::Running) || job.cancellation.is_some() {
        return Ok(());
    }
    let active: Vec<Uuid> = sqlx::query_scalar("SELECT a.id FROM attempts a JOIN tasks t ON t.id=a.task_id WHERE t.job_id=$1 AND a.state IN ('Granted','Running') ORDER BY a.id")
        .bind(job_id.as_uuid()).fetch_all(&mut *tx).await?;
    for id in active {
        let (attempt, _) =
            attempts::lock_attempt(&mut tx, bergsmith_common::ids::AttemptId::from_uuid(id))
                .await?;
        let session_live =
            workers::attempt_session_is_live(&mut tx, attempt.worker_session_id).await?;
        let live: bool = sqlx::query_scalar("SELECT $1::timestamptz > clock_timestamp()")
            .bind(attempt.lease_expires_at)
            .fetch_one(&mut *tx)
            .await?;
        if live && session_live {
            continue;
        }
        let retry = attempt.generation < 3;
        let deadline = if retry {
            Some(retry_deadline(&mut tx, attempt.task_id, attempt.generation).await?)
        } else {
            None
        };
        attempts::end_attempt(
            &mut tx,
            &attempts::EndAttempt {
                attempt_id: attempt.id,
                generation: attempt.generation,
                attempt_state: bergsmith_common::task::AttemptState::Expired,
                task_state: if retry {
                    bergsmith_common::task::TaskState::RetryWaiting
                } else {
                    bergsmith_common::task::TaskState::Failed
                },
                retry_not_before: deadline,
            },
        )
        .await?;
        if !retry {
            stop_locked(
                &mut tx,
                &job,
                JobState::Failed,
                "ExecutionRetryBudgetExhausted",
            )
            .await?;
            leadership::ensure_leadership(&mut tx, epoch).await?;
            tx.commit().await?;
            return Ok(());
        }
    }
    if release {
        sqlx::query("UPDATE tasks SET state='Pending',retry_not_before=NULL,updated_at=now() WHERE job_id=$1 AND state='RetryWaiting' AND retry_not_before<=clock_timestamp()")
            .bind(job_id.as_uuid()).execute(&mut *tx).await?;
    }
    leadership::ensure_leadership(&mut tx, epoch).await?;
    tx.commit().await?;
    Ok(())
}

async fn stop_locked(
    conn: &mut PgConnection,
    job: &jobs::Job,
    state: JobState,
    reason: &str,
) -> Result<(), PersistenceError> {
    jobs::transition_job(
        conn,
        &jobs::Transition {
            job_id: job.id,
            expected_revision: job.state_revision,
            to: state,
            reason: Some(reason.into()),
            actor: None,
            non_publication_proven: false,
        },
    )
    .await?;
    Ok(())
}

/// Finishes cancellation or validation failure without inventing publication proof.
///
/// The stop applies only if the job is still at `expected_revision`, the
/// revision whose state the caller's decision was based on. A job that moved
/// meanwhile (for example to `ReadyToCommit`, `Committing`, or `Cancelling`)
/// is left unchanged and `Ok(false)` is returned; a later pass decides again
/// from its new state. Terminal jobs are never changed.
///
/// # Errors
/// Rejects stale leadership and invalid lifecycle transitions; authorized publications stay unresolved.
pub async fn stop(
    conn: &mut PgConnection,
    job_id: JobId,
    expected_revision: i64,
    epoch: i64,
    state: JobState,
    reason: &str,
) -> Result<bool, PersistenceError> {
    let mut tx = conn.begin().await?;
    let job = jobs::lock_job(&mut tx, job_id).await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    if job.state.is_terminal() || job.state_revision != expected_revision {
        return Ok(false);
    }
    stop_locked(&mut tx, &job, state, reason).await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    tx.commit().await?;
    Ok(true)
}

/// Checks leadership inside a short transaction before external reconciliation work.
///
/// # Errors
/// Returns lost leadership or database errors.
pub async fn check_leadership(conn: &mut PgConnection, epoch: i64) -> Result<(), PersistenceError> {
    let mut tx = conn.begin().await?;
    leadership::ensure_leadership(&mut tx, epoch).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_has_stable_bounded_exponential_equal_jitter() {
        let policy = RetryPolicy {
            base_millis: 1_000,
            max_millis: 1_500,
        };
        assert!(policy.is_valid());
        for value in [0, 1, 127, u128::MAX] {
            let task = TaskId::from_uuid(Uuid::from_u128(value));
            let first = policy.delay_millis(task, 1);
            let second = policy.delay_millis(task, 2);
            assert!((500..=1_000).contains(&first));
            assert!((750..=1_500).contains(&second));
            assert_eq!(first, policy.delay_millis(task, 1));
        }
        assert!(
            !RetryPolicy {
                base_millis: 0,
                max_millis: 1
            }
            .is_valid()
        );
        assert!(
            !RetryPolicy {
                base_millis: 2,
                max_millis: 1
            }
            .is_valid()
        );
        assert!(
            !RetryPolicy {
                base_millis: 1,
                max_millis: 86_400_001
            }
            .is_valid()
        );
        assert!(
            !AdmissionLimits {
                attempts: 0,
                ..AdmissionLimits::default()
            }
            .is_valid()
        );
    }
}
