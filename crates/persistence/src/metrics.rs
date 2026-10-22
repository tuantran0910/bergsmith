//! Read-only ledger aggregates for operational metrics (BS-020.T2).
//!
//! Every value is derived from durable rows, so it survives restarts and
//! leadership changes without process-local counters. Counters (`*_total`)
//! count immutable or monotonic facts: terminal jobs never transition, and
//! attempt failures, receipts, and output objects are immutable. Gauges
//! describe current state. The snapshot is read in one read-only transaction
//! with a statement timeout, so a slow ledger cannot stall a scrape.

use std::collections::BTreeMap;
use std::time::Duration;

use sqlx::{Connection, PgConnection};

use crate::PersistenceError;

/// A consistent view of the ledger at one database instant.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LedgerSnapshot {
    /// Non-terminal jobs by state.
    pub active_jobs: BTreeMap<String, i64>,
    /// Terminal jobs by state (monotonic).
    pub ended_jobs: BTreeMap<String, i64>,
    /// Tasks waiting for a worker.
    pub pending_tasks: i64,
    /// Seconds the longest-waiting pending task has waited; zero when none.
    pub oldest_pending_task_seconds: f64,
    /// Tasks waiting for a retry timer.
    pub retry_waiting_tasks: i64,
    /// Granted or running attempts whose lease is current.
    pub running_attempts: i64,
    /// Granted or running attempts whose lease expired but which are not
    /// fenced yet.
    pub expired_lease_attempts: i64,
    /// Attempts ever started (monotonic); each consumes retry budget.
    pub attempts_started: i64,
    /// Attempt failures by stable reason code and disposition (monotonic).
    pub attempt_failures: BTreeMap<(String, String), i64>,
    /// Planned input bytes over every persisted plan (monotonic).
    pub planned_input_bytes: i64,
    /// Accepted output bytes by the outcome of their job: `published`,
    /// `conflict` (`ReplanRequired`), `abandoned` (other terminal states),
    /// or `pending` (monotonic in total).
    pub output_bytes: BTreeMap<String, i64>,
    /// Attempt output prefixes of ended jobs that no publication uses:
    /// failed, expired, or cancelled attempts and every attempt of a job that
    /// did not succeed. Bergsmith never deletes them.
    pub retained_unused_prefixes: i64,
    /// Jobs whose send was authorized and whose outcome is not yet recorded
    /// (`Committing` after authorization, or `CommitOutcomeUnknown`).
    pub unresolved_publications: i64,
    /// Seconds since the oldest unresolved publication was authorized.
    pub oldest_unresolved_publication_seconds: f64,
    /// Jobs whose cancellation was requested (monotonic).
    pub cancellation_requests: i64,
    /// Cancellations requested after publication started, which therefore
    /// could not prevent it (monotonic).
    pub cancellation_requests_too_late: i64,
    /// Schedule occurrences by state (monotonic per state except `Pending`).
    pub schedule_occurrences: BTreeMap<String, i64>,
    /// Occurrences that folded later due instants missed during downtime.
    pub coalesced_downtime_occurrences: i64,
    /// Seconds the most overdue enabled schedule is past its due instant;
    /// zero when none is overdue.
    pub schedule_lag_seconds: f64,
    /// Current worker sessions by `live`, `draining`, or `expired`.
    pub worker_sessions: BTreeMap<String, i64>,
}

/// Reads a [`LedgerSnapshot`] in one read-only transaction.
///
/// `timeout` bounds every statement. Aggregates scan their tables, so callers
/// should cache the result rather than read it on every request.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the database is unavailable or a
/// statement exceeds `timeout`.
pub async fn ledger_snapshot(
    conn: &mut PgConnection,
    timeout: Duration,
) -> Result<LedgerSnapshot, PersistenceError> {
    let mut tx = conn.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let millis = i64::try_from(timeout.as_millis())
        .unwrap_or(i64::MAX)
        .max(1);
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind(millis.to_string())
        .execute(&mut *tx)
        .await?;

    let mut snapshot = LedgerSnapshot::default();
    jobs(&mut tx, &mut snapshot).await?;
    tasks_and_attempts(&mut tx, &mut snapshot).await?;
    failures(&mut tx, &mut snapshot).await?;
    work(&mut tx, &mut snapshot).await?;
    publications(&mut tx, &mut snapshot).await?;
    schedules(&mut tx, &mut snapshot).await?;
    sessions(&mut tx, &mut snapshot).await?;
    tx.commit().await?;
    Ok(snapshot)
}

async fn jobs(
    conn: &mut PgConnection,
    snapshot: &mut LedgerSnapshot,
) -> Result<(), PersistenceError> {
    let jobs: Vec<(String, i64)> =
        sqlx::query_as("SELECT state, count(*) FROM jobs GROUP BY state")
            .fetch_all(&mut *conn)
            .await?;
    for (state, count) in jobs {
        let terminal = matches!(
            state.as_str(),
            "Succeeded" | "NoOp" | "Cancelled" | "ReplanRequired" | "Failed"
        );
        if terminal {
            snapshot.ended_jobs.insert(state, count);
        } else {
            snapshot.active_jobs.insert(state, count);
        }
    }
    Ok(())
}

async fn tasks_and_attempts(
    conn: &mut PgConnection,
    snapshot: &mut LedgerSnapshot,
) -> Result<(), PersistenceError> {
    let (pending, oldest, retry_waiting): (i64, f64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE state = 'Pending'),
                COALESCE(EXTRACT(EPOCH FROM now() - min(updated_at)
                    FILTER (WHERE state = 'Pending'))::float8, 0),
                count(*) FILTER (WHERE state = 'RetryWaiting')
         FROM tasks WHERE state IN ('Pending', 'RetryWaiting')",
    )
    .fetch_one(&mut *conn)
    .await?;
    snapshot.pending_tasks = pending;
    snapshot.oldest_pending_task_seconds = oldest.max(0.0);
    snapshot.retry_waiting_tasks = retry_waiting;

    let (running, expired, started): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE state IN ('Granted', 'Running')
                                   AND lease_expires_at > now()),
                count(*) FILTER (WHERE state IN ('Granted', 'Running')
                                   AND lease_expires_at <= now()),
                count(*)
         FROM attempts",
    )
    .fetch_one(&mut *conn)
    .await?;
    snapshot.running_attempts = running;
    snapshot.expired_lease_attempts = expired;
    snapshot.attempts_started = started;
    Ok(())
}

async fn failures(
    conn: &mut PgConnection,
    snapshot: &mut LedgerSnapshot,
) -> Result<(), PersistenceError> {
    let failures: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT reason_code, disposition, count(*) FROM attempt_failures
         GROUP BY reason_code, disposition",
    )
    .fetch_all(&mut *conn)
    .await?;
    snapshot.attempt_failures = failures
        .into_iter()
        .map(|(reason, disposition, count)| ((reason, disposition), count))
        .collect();
    Ok(())
}

async fn work(
    conn: &mut PgConnection,
    snapshot: &mut LedgerSnapshot,
) -> Result<(), PersistenceError> {
    snapshot.planned_input_bytes = sqlx::query_scalar(
        "SELECT COALESCE(sum((content #>> '{estimate,bytes}')::numeric), 0)::bigint FROM plans",
    )
    .fetch_one(&mut *conn)
    .await?;

    let outputs: Vec<(String, i64)> = sqlx::query_as(
        "SELECT CASE
                    WHEN j.state = 'Succeeded' THEN 'published'
                    WHEN j.state = 'ReplanRequired' THEN 'conflict'
                    WHEN j.state IN ('NoOp', 'Cancelled', 'Failed') THEN 'abandoned'
                    ELSE 'pending'
                END AS outcome,
                sum(o.size_bytes)::bigint
         FROM attempt_output_objects o
         JOIN attempts a ON a.id = o.attempt_id
         JOIN tasks t ON t.id = a.task_id
         JOIN jobs j ON j.id = t.job_id
         GROUP BY outcome",
    )
    .fetch_all(&mut *conn)
    .await?;
    snapshot.output_bytes = outputs.into_iter().collect();

    snapshot.retained_unused_prefixes = sqlx::query_scalar(
        "SELECT count(*) FROM attempt_output_prefixes p
         JOIN attempts a ON a.id = p.attempt_id
         JOIN tasks t ON t.id = a.task_id
         JOIN jobs j ON j.id = t.job_id
         WHERE j.state IN ('NoOp', 'Cancelled', 'ReplanRequired', 'Failed')
            OR (j.state = 'Succeeded' AND a.state <> 'Accepted')",
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(())
}

async fn publications(
    conn: &mut PgConnection,
    snapshot: &mut LedgerSnapshot,
) -> Result<(), PersistenceError> {
    let (unresolved, oldest, requested, too_late): (i64, f64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE state = 'CommitOutcomeUnknown'
                    OR (state = 'Committing' AND publication_started_at IS NOT NULL)),
                COALESCE(EXTRACT(EPOCH FROM now() - min(publication_started_at)
                    FILTER (WHERE state IN ('Committing', 'CommitOutcomeUnknown')))::float8, 0),
                count(*) FILTER (WHERE cancellation_requested_at IS NOT NULL),
                count(*) FILTER (WHERE cancellation_requested_at > publication_started_at)
         FROM jobs",
    )
    .fetch_one(&mut *conn)
    .await?;
    snapshot.unresolved_publications = unresolved;
    snapshot.oldest_unresolved_publication_seconds = oldest.max(0.0);
    snapshot.cancellation_requests = requested;
    snapshot.cancellation_requests_too_late = too_late;
    Ok(())
}

async fn schedules(
    conn: &mut PgConnection,
    snapshot: &mut LedgerSnapshot,
) -> Result<(), PersistenceError> {
    let occurrences: Vec<(String, i64)> = sqlx::query_as(
        "SELECT state, count(*) FROM maintenance_schedule_occurrences GROUP BY state",
    )
    .fetch_all(&mut *conn)
    .await?;
    snapshot.schedule_occurrences = occurrences.into_iter().collect();
    snapshot.coalesced_downtime_occurrences = sqlx::query_scalar(
        "SELECT count(*) FROM maintenance_schedule_occurrences WHERE coalesced_until IS NOT NULL",
    )
    .fetch_one(&mut *conn)
    .await?;
    let lag: f64 = sqlx::query_scalar(
        "SELECT COALESCE(EXTRACT(EPOCH FROM now() - min(next_due_at))::float8, 0)
         FROM maintenance_schedule_state WHERE next_due_at < now()",
    )
    .fetch_one(&mut *conn)
    .await?;
    snapshot.schedule_lag_seconds = lag.max(0.0);
    Ok(())
}

async fn sessions(
    conn: &mut PgConnection,
    snapshot: &mut LedgerSnapshot,
) -> Result<(), PersistenceError> {
    let sessions: Vec<(String, i64)> = sqlx::query_as(
        "SELECT CASE
                    WHEN lease_expires_at <= now() THEN 'expired'
                    WHEN draining THEN 'draining'
                    ELSE 'live'
                END AS status,
                count(*)
         FROM worker_sessions WHERE NOT superseded GROUP BY status",
    )
    .fetch_all(&mut *conn)
    .await?;
    snapshot.worker_sessions = sessions.into_iter().collect();
    Ok(())
}
