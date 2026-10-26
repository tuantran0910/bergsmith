//! Ledger aggregates for the landing overview (BS-027.T3, BS-032.T7).
//!
//! The overview answers "what needs attention?" from durable rows only: the
//! outcomes of terminal jobs in the last day, jobs whose publication is
//! unresolved, tables whose newest terminal job needs a fresh plan or a fix,
//! schedule evaluations that are late or were folded, and registered worker
//! capacity. Job and table figures apply the same grant predicate as every
//! job read, so the page cannot reveal a table its caller may not read.

use std::collections::BTreeMap;

use bergsmith_common::ids::{JobId, OrganizationId, PrincipalId, TableId};
use bergsmith_common::job::JobState;
use chrono::{DateTime, Utc};
use sqlx::PgConnection;

use crate::PersistenceError;
use crate::grants::table_visible;
use crate::jobs::parse_job_state;

/// Why one job needs attention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttentionReason {
    /// The send was authorized and the outcome is not recorded yet.
    UnresolvedPublication,
    /// The plan cannot publish over the table as it changed.
    ReplanRequired,
    /// The job failed.
    Failed,
}

impl AttentionReason {
    /// The stable API name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnresolvedPublication => "unresolved_publication",
            Self::ReplanRequired => "replan_required",
            Self::Failed => "failed",
        }
    }
}

/// One job the landing page should surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttentionJob {
    /// The job.
    pub job_id: JobId,
    /// The table it belongs to.
    pub table_id: TableId,
    /// Its current state.
    pub state: JobState,
    /// Why it is listed.
    pub reason: AttentionReason,
    /// Seconds since the publication was authorized, or since it ended.
    pub age_seconds: i64,
}

/// Terminal jobs in the last day, by outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JobOutcomes {
    /// Jobs that ended `Succeeded`.
    pub succeeded: i64,
    /// Jobs that ended `NoOp`.
    pub no_op: i64,
    /// Jobs that ended `Cancelled`.
    pub cancelled: i64,
    /// Jobs that ended `ReplanRequired`.
    pub replanned: i64,
    /// Jobs that ended `Failed`.
    pub failed: i64,
}

impl JobOutcomes {
    /// Every terminal job counted.
    #[must_use]
    pub const fn ended(self) -> i64 {
        self.succeeded + self.no_op + self.cancelled + self.replanned + self.failed
    }

    /// The outcomes as a map, in state order.
    #[must_use]
    pub fn by_state(self) -> BTreeMap<&'static str, i64> {
        BTreeMap::from([
            ("Succeeded", self.succeeded),
            ("NoOp", self.no_op),
            ("Cancelled", self.cancelled),
            ("ReplanRequired", self.replanned),
            ("Failed", self.failed),
        ])
    }
}

/// Counts the attention backlog independently of the bounded item list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttentionSummary {
    /// Jobs whose send was authorized and whose outcome is not recorded.
    pub unresolved_publications: i64,
    /// Seconds since the oldest unresolved publication was authorized.
    pub oldest_unresolved_age_seconds: i64,
    /// Replanned or failed jobs inside the window, by terminal transition.
    pub terminal_jobs: i64,
}

/// Counts the jobs that need attention: unresolved publications without a
/// bound, and terminal jobs in the window.
///
/// This is separate from [`attention`] because that list is bounded. A page
/// that counts its own rows would report five unresolved publications when
/// six exist.
///
/// # Errors
///
/// [`PersistenceError`] when the query fails.
pub async fn attention_summary(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    roles: &[String],
    since: DateTime<Utc>,
) -> Result<AttentionSummary, PersistenceError> {
    let sql = concat!(
        "WITH attention AS ( \
             SELECT j.state, \
                    EXTRACT(EPOCH FROM (now() - \
                        CASE WHEN j.state IN ('Committing', 'CommitOutcomeUnknown') \
                             THEN COALESCE(j.publication_started_at, j.updated_at) \
                             ELSE j.updated_at END)) AS age_seconds \
             FROM jobs j \
             JOIN table_identities t \
               ON t.id = j.table_identity_id AND t.organization_id = j.organization_id \
             WHERE j.organization_id = $1 \
               AND (j.state IN ('Committing', 'CommitOutcomeUnknown') \
                    OR (j.state IN ('ReplanRequired', 'Failed') AND j.updated_at >= $4)) \
               AND ",
        table_visible!(),
        " ) \
         SELECT count(*) FILTER ( \
                    WHERE state IN ('Committing', 'CommitOutcomeUnknown'))::bigint \
                    AS unresolved_publications, \
                COALESCE(FLOOR(max(age_seconds) FILTER ( \
                    WHERE state IN ('Committing', 'CommitOutcomeUnknown'))), 0)::bigint \
                    AS oldest_unresolved_age_seconds, \
                count(*) FILTER ( \
                    WHERE state IN ('ReplanRequired', 'Failed'))::bigint AS terminal_jobs \
         FROM attention"
    );
    let row: AttentionSummaryRow = sqlx::query_as(sql)
        .bind(organization_id.as_uuid())
        .bind(principal_id.as_uuid())
        .bind(roles)
        .bind(since)
        .fetch_one(conn)
        .await?;
    Ok(AttentionSummary {
        unresolved_publications: row.unresolved_publications,
        oldest_unresolved_age_seconds: row.oldest_unresolved_age_seconds,
        terminal_jobs: row.terminal_jobs,
    })
}

/// Inventory coverage and the tables whose newest job needs a decision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TableOverview {
    /// Visible tables.
    pub total: i64,
    /// Visible tables without a statistics sample.
    pub never_sampled: i64,
    /// Visible tables whose newest terminal job in the window replanned or
    /// failed.
    pub latest_job_attention: i64,
}

/// Schedule evaluations that are late, folded, or refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScheduleOverview {
    /// Schedules in the organization.
    pub total: i64,
    /// Schedules that are enabled.
    pub enabled: i64,
    /// Enabled schedules whose next due instant has passed. Admission commits
    /// an occurrence and finishes it in one transaction, so a late evaluation
    /// is visible here even while the scheduler cannot run.
    pub overdue: i64,
    /// Seconds the oldest overdue schedule has waited; zero when none.
    pub oldest_overdue_age_seconds: i64,
    /// Occurrences folded into a later evaluation in the last day.
    pub coalesced: i64,
    /// Occurrences refused in the last day.
    pub skipped: i64,
}

/// Registered worker capacity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkerOverview {
    /// Sessions that are not superseded.
    pub sessions: i64,
    /// Sessions whose lease is current and which are not draining.
    pub live_sessions: i64,
    /// Memory the live sessions registered, in bytes.
    pub memory_bytes: i64,
    /// CPU millicores the live sessions registered.
    pub cpu_millis: i64,
    /// Attempts those sessions are running.
    pub running_attempts: i64,
}

/// Counts terminal jobs in the window that ended each way.
///
/// # Errors
///
/// [`PersistenceError`] when the query fails.
pub async fn job_outcomes(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    roles: &[String],
    since: DateTime<Utc>,
) -> Result<JobOutcomes, PersistenceError> {
    let sql = concat!(
        "SELECT \
            count(*) FILTER (WHERE j.state = 'Succeeded')::bigint AS succeeded, \
            count(*) FILTER (WHERE j.state = 'NoOp')::bigint AS no_op, \
            count(*) FILTER (WHERE j.state = 'Cancelled')::bigint AS cancelled, \
            count(*) FILTER (WHERE j.state = 'ReplanRequired')::bigint AS replanned, \
            count(*) FILTER (WHERE j.state = 'Failed')::bigint AS failed \
         FROM jobs j \
         JOIN table_identities t \
           ON t.id = j.table_identity_id AND t.organization_id = j.organization_id \
         WHERE j.organization_id = $1 \
           AND j.state IN ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed') \
           AND j.updated_at >= $4 \
           AND ",
        table_visible!()
    );
    let row: OutcomesRow = sqlx::query_as(sql)
        .bind(organization_id.as_uuid())
        .bind(principal_id.as_uuid())
        .bind(roles)
        .bind(since)
        .fetch_one(conn)
        .await?;
    Ok(JobOutcomes {
        succeeded: row.succeeded,
        no_op: row.no_op,
        cancelled: row.cancelled,
        replanned: row.replanned,
        failed: row.failed,
    })
}

/// The jobs that need attention, oldest first, bounded by `limit`.
///
/// Unresolved publications come first because they block new work on their
/// table; replanned and failed jobs follow. Both ages are measured from the
/// event that matters: an unresolved publication from its send authorization,
/// a terminal job from its terminal transition. The window bounds the terminal
/// states by that transition, so an old failure does not sit on the page
/// forever while a job created earlier and ended today still counts.
///
/// # Errors
///
/// [`PersistenceError`] when the query fails or a stored state is unknown.
pub async fn attention(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    roles: &[String],
    since: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<AttentionJob>, PersistenceError> {
    let sql = concat!(
        "SELECT j.id, j.table_identity_id, j.state, \
                GREATEST(0, FLOOR(EXTRACT(EPOCH FROM (now() - \
                    CASE WHEN j.state IN ('Committing', 'CommitOutcomeUnknown') \
                         THEN COALESCE(j.publication_started_at, j.updated_at) \
                         ELSE j.updated_at END))))::bigint AS age_seconds \
         FROM jobs j \
         JOIN table_identities t \
           ON t.id = j.table_identity_id AND t.organization_id = j.organization_id \
         WHERE j.organization_id = $1 \
           AND (j.state IN ('Committing', 'CommitOutcomeUnknown') \
                OR (j.state IN ('ReplanRequired', 'Failed') AND j.updated_at >= $4)) \
           AND ",
        table_visible!(),
        " ORDER BY CASE WHEN j.state IN ('Committing', 'CommitOutcomeUnknown') THEN 0 ELSE 1 END, \
                age_seconds DESC, j.id \
         LIMIT $5"
    );
    let rows: Vec<AttentionRow> = sqlx::query_as(sql)
        .bind(organization_id.as_uuid())
        .bind(principal_id.as_uuid())
        .bind(roles)
        .bind(since)
        .bind(limit)
        .fetch_all(conn)
        .await?;
    rows.into_iter().map(AttentionRow::into_attention).collect()
}

/// Inventory coverage and the tables whose newest job needs a decision.
///
/// # Errors
///
/// [`PersistenceError`] when the query fails.
pub async fn tables(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    roles: &[String],
    since: DateTime<Utc>,
) -> Result<TableOverview, PersistenceError> {
    let sql = concat!(
        "WITH visible AS ( \
             SELECT t.id FROM table_identities t \
             WHERE t.organization_id = $1 AND ",
        table_visible!(),
        " ), latest AS ( \
             SELECT DISTINCT ON (j.table_identity_id) j.table_identity_id, j.state \
             FROM jobs j \
             JOIN visible v ON v.id = j.table_identity_id \
             WHERE j.state IN ('Succeeded', 'NoOp', 'Cancelled', 'ReplanRequired', 'Failed') \
               AND j.updated_at >= $4 \
             ORDER BY j.table_identity_id, j.id DESC \
         ) \
         SELECT (SELECT count(*) FROM visible)::bigint AS total, \
                (SELECT count(*) FROM visible v WHERE NOT EXISTS ( \
                    SELECT 1 FROM table_statistics s \
                    WHERE s.organization_id = $1 AND s.table_identity_id = v.id))::bigint \
                    AS never_sampled, \
                (SELECT count(*) FROM latest \
                    WHERE state IN ('ReplanRequired', 'Failed'))::bigint \
                    AS latest_job_attention"
    );
    let row: TablesRow = sqlx::query_as(sql)
        .bind(organization_id.as_uuid())
        .bind(principal_id.as_uuid())
        .bind(roles)
        .bind(since)
        .fetch_one(conn)
        .await?;
    Ok(TableOverview {
        total: row.total,
        never_sampled: row.never_sampled,
        latest_job_attention: row.latest_job_attention,
    })
}

/// Schedule evaluations that are late, folded, or refused.
///
/// # Errors
///
/// [`PersistenceError`] when the query fails.
pub async fn schedules(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
) -> Result<ScheduleOverview, PersistenceError> {
    let row: SchedulesRow = sqlx::query_as(
        "WITH s AS ( \
             SELECT id, enabled FROM maintenance_schedules WHERE organization_id = $1 \
         ), st AS ( \
             SELECT schedule_id, next_due_at FROM maintenance_schedule_state \
             WHERE organization_id = $1 \
         ), o AS ( \
             SELECT state, scheduled_for, created_at \
             FROM maintenance_schedule_occurrences WHERE organization_id = $1 \
         ) \
         SELECT (SELECT count(*) FROM s)::bigint AS total, \
                (SELECT count(*) FROM s WHERE enabled)::bigint AS enabled, \
                (SELECT count(*) FROM s JOIN st ON st.schedule_id = s.id \
                 WHERE s.enabled AND st.next_due_at <= now())::bigint AS overdue, \
                (SELECT COALESCE(FLOOR(max(EXTRACT(EPOCH FROM (now() - st.next_due_at)))), 0)::bigint \
                 FROM s JOIN st ON st.schedule_id = s.id \
                 WHERE s.enabled AND st.next_due_at <= now()) AS oldest_overdue_age_seconds, \
                (SELECT count(*) FROM o \
                 WHERE state = 'Coalesced' AND created_at >= now() - interval '24 hours')::bigint \
                    AS coalesced, \
                (SELECT count(*) FROM o \
                 WHERE state = 'Skipped' AND created_at >= now() - interval '24 hours')::bigint \
                    AS skipped",
    )
    .bind(organization_id.as_uuid())
    .fetch_one(conn)
    .await?;
    Ok(ScheduleOverview {
        total: row.total,
        enabled: row.enabled,
        overdue: row.overdue,
        oldest_overdue_age_seconds: row.oldest_overdue_age_seconds,
        coalesced: row.coalesced,
        skipped: row.skipped,
    })
}

/// Registered worker capacity.
///
/// # Errors
///
/// [`PersistenceError`] when the query fails.
pub async fn workers(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
) -> Result<WorkerOverview, PersistenceError> {
    let row: WorkersRow = sqlx::query_as(
        "SELECT count(*)::bigint AS sessions, \
                count(*) FILTER (WHERE lease_expires_at > now() AND NOT draining)::bigint \
                    AS live_sessions, \
                COALESCE(sum(memory_bytes) FILTER ( \
                    WHERE lease_expires_at > now() AND NOT draining), 0)::bigint AS memory_bytes, \
                COALESCE(sum(cpu_millis) FILTER ( \
                    WHERE lease_expires_at > now() AND NOT draining), 0)::bigint AS cpu_millis, \
                (SELECT count(*) FROM attempts a \
                 JOIN worker_sessions w ON w.id = a.worker_session_id \
                 WHERE w.organization_id = $1 \
                   AND a.state IN ('Granted', 'Running'))::bigint AS running_attempts \
         FROM worker_sessions \
         WHERE organization_id = $1 AND NOT superseded",
    )
    .bind(organization_id.as_uuid())
    .fetch_one(conn)
    .await?;
    Ok(WorkerOverview {
        sessions: row.sessions,
        live_sessions: row.live_sessions,
        memory_bytes: row.memory_bytes,
        cpu_millis: row.cpu_millis,
        running_attempts: row.running_attempts,
    })
}

#[derive(sqlx::FromRow)]
struct OutcomesRow {
    succeeded: i64,
    no_op: i64,
    cancelled: i64,
    replanned: i64,
    failed: i64,
}

#[derive(sqlx::FromRow)]
struct AttentionRow {
    id: uuid::Uuid,
    table_identity_id: uuid::Uuid,
    state: String,
    age_seconds: i64,
}

impl AttentionRow {
    fn into_attention(self) -> Result<AttentionJob, PersistenceError> {
        let state = parse_job_state(&self.state)?;
        let reason = match state {
            JobState::Committing | JobState::CommitOutcomeUnknown => {
                AttentionReason::UnresolvedPublication
            }
            JobState::ReplanRequired => AttentionReason::ReplanRequired,
            JobState::Failed => AttentionReason::Failed,
            _ => {
                return Err(PersistenceError::CorruptValue {
                    entity: "overview attention job",
                    value: self.state,
                });
            }
        };
        Ok(AttentionJob {
            job_id: JobId::from_uuid(self.id),
            table_id: TableId::from_uuid(self.table_identity_id),
            state,
            reason,
            age_seconds: self.age_seconds,
        })
    }
}

#[derive(sqlx::FromRow)]
struct AttentionSummaryRow {
    unresolved_publications: i64,
    oldest_unresolved_age_seconds: i64,
    terminal_jobs: i64,
}

#[derive(sqlx::FromRow)]
struct TablesRow {
    total: i64,
    never_sampled: i64,
    latest_job_attention: i64,
}

#[derive(sqlx::FromRow)]
struct SchedulesRow {
    total: i64,
    enabled: i64,
    overdue: i64,
    oldest_overdue_age_seconds: i64,
    coalesced: i64,
    skipped: i64,
}

#[derive(sqlx::FromRow)]
struct WorkersRow {
    sessions: i64,
    live_sessions: i64,
    memory_bytes: i64,
    cpu_millis: i64,
    running_attempts: i64,
}
