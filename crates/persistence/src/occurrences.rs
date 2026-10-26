//! Durable schedule occurrence identity and due-time admission (BS-019.T2).
//!
//! One pending evaluation is persisted as an occurrence row before the job it
//! may create. The row is keyed by the first due instant it represents, so a
//! replayed evaluation cannot admit a second job. When the coordinator was
//! down for several instants, the earlier ones are folded into the same row
//! (`coalesced_until` records the last one) instead of producing a burst of
//! jobs.
//!
//! Admission rechecks everything the plan requires at that moment: the
//! schedule and policy are enabled, the service principal still holds
//! `jobs:submit` for the table, the instant is inside the schedule's
//! maintenance window, and no other job already owns the table. A schedule
//! that is disabled has no state row, so it cannot become due.
//!
//! An evaluation outside its window is routine, so it is recorded as one
//! skipped row covering the due span and the cursor jumps to the first
//! occurrence at or after the next opening. It is not audited: a disabled
//! policy or an ineligible service identity is an operator problem worth an
//! audit record, while a tick outside the quiet interval is the window doing
//! its job.

use bergsmith_common::audit::{ActorKind, AuditAction, AuditOutcome};
use bergsmith_common::health::{HealthThresholds, HealthTrigger, HealthVerdict};
use bergsmith_common::ids::{JobId, OrganizationId, PolicyId, PrincipalId, ScheduleId, TableId};
use bergsmith_common::schedule::CronSchedule;
use bergsmith_common::window::MaintenanceWindow;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::audit::NewAuditEvent;
use crate::jobs::{self, NewJob};
use crate::tenancy::ExecutionRoute;
use crate::{PersistenceError, audit, schedules};

/// Upper bound for a due scan and an occurrence listing.
const MAX_PAGE: i64 = 1_000;

/// The stable persisted reason for an occurrence evaluated outside its window.
pub const OUTSIDE_WINDOW_REASON: &str = "OutsideWindow";

/// The stable persisted reason for an occurrence whose health trigger is unmet.
pub const HEALTH_TRIGGER_REASON: &str = "HealthTriggerNotMet";

/// Why admission skipped an evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SkipReason {
    /// No enabled connection is usable by the service identity.
    ConnectionDisabled,
    /// Every enabled connection has maintenance paused.
    MaintenancePaused,
    /// The policy is disabled.
    PolicyDisabled,
    /// The service principal no longer satisfies the execution grant.
    ServiceIdentityIneligible,
    /// The evaluation fell outside the schedule's maintenance window.
    OutsideWindow,
    /// The policy's health trigger is not met by the table's findings.
    HealthTriggerNotMet,
}

impl SkipReason {
    /// The stable reason persisted on the occurrence and in the audit record.
    const fn as_str(self) -> &'static str {
        match self {
            Self::ConnectionDisabled => "ConnectionDisabled",
            Self::MaintenancePaused => "MaintenancePaused",
            Self::PolicyDisabled => "PolicyDisabled",
            Self::ServiceIdentityIneligible => "ServiceIdentityIneligible",
            Self::OutsideWindow => OUTSIDE_WINDOW_REASON,
            Self::HealthTriggerNotMet => HEALTH_TRIGGER_REASON,
        }
    }

    /// Whether the skip is an operator problem worth an audit record.
    ///
    /// A tick outside the maintenance window is expected behavior for a
    /// schedule whose cron is more frequent than its window, so auditing every
    /// one would bury real denials. The occurrence row still records the span.
    const fn audited(self) -> bool {
        !matches!(self, Self::OutsideWindow | Self::HealthTriggerNotMet)
    }
}

/// Durable state of one schedule evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OccurrenceState {
    /// The row exists and the admission decision is still in progress.
    Pending,
    /// A job was created for this evaluation.
    Admitted,
    /// The table already had a nonterminal job; this evaluation folded into it.
    Coalesced,
    /// Admission was refused, for example because the policy is disabled.
    Skipped,
}

impl OccurrenceState {
    /// Returns the persisted name of the state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Admitted => "Admitted",
            Self::Coalesced => "Coalesced",
            Self::Skipped => "Skipped",
        }
    }

    fn parse(value: &str) -> Result<Self, PersistenceError> {
        match value {
            "Pending" => Ok(Self::Pending),
            "Admitted" => Ok(Self::Admitted),
            "Coalesced" => Ok(Self::Coalesced),
            "Skipped" => Ok(Self::Skipped),
            other => Err(PersistenceError::CorruptValue {
                entity: "schedule occurrence state",
                value: other.to_owned(),
            }),
        }
    }
}

/// One durable schedule evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occurrence {
    /// Identifier.
    pub id: Uuid,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// The schedule that produced it.
    pub schedule_id: ScheduleId,
    /// First due instant the evaluation represents.
    pub scheduled_for: DateTime<Utc>,
    /// Last due instant folded into the evaluation during downtime, if any.
    pub coalesced_until: Option<DateTime<Utc>>,
    /// Current state.
    pub state: OccurrenceState,
    /// The admitted job, or the nonterminal job that already owns the table.
    pub job_id: Option<JobId>,
    /// Stable reason for `Coalesced` and `Skipped`.
    pub reason: Option<String>,
    /// When the row was written (database time).
    pub created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct OccurrenceRow {
    id: Uuid,
    organization_id: Uuid,
    schedule_id: Uuid,
    scheduled_for: DateTime<Utc>,
    coalesced_until: Option<DateTime<Utc>>,
    state: String,
    job_id: Option<Uuid>,
    reason: Option<String>,
    created_at: DateTime<Utc>,
}

impl TryFrom<OccurrenceRow> for Occurrence {
    type Error = PersistenceError;

    fn try_from(row: OccurrenceRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            organization_id: OrganizationId::from_uuid(row.organization_id),
            schedule_id: ScheduleId::from_uuid(row.schedule_id),
            scheduled_for: row.scheduled_for,
            coalesced_until: row.coalesced_until,
            state: OccurrenceState::parse(&row.state)?,
            job_id: row.job_id.map(JobId::from_uuid),
            reason: row.reason,
            created_at: row.created_at,
        })
    }
}

/// Columns selected for [`Occurrence`], in [`OccurrenceRow`] order.
macro_rules! occurrence_columns {
    () => {
        "id, organization_id, schedule_id, scheduled_for, coalesced_until, \
         state, job_id, reason, created_at"
    };
}

#[derive(sqlx::FromRow)]
struct DueSchedule {
    id: Uuid,
    organization_id: Uuid,
    policy_id: Uuid,
    table_identity_id: Uuid,
    cron_expression: String,
    time_zone: String,
    window_start_minute: Option<i32>,
    window_end_minute: Option<i32>,
    service_principal_id: Uuid,
    enabled: bool,
    policy_enabled: bool,
    health_trigger: Option<Value>,
}

/// Returns the enabled schedules whose next instant is at or before `now`.
///
/// # Errors
///
/// Returns a database error if the read fails.
pub async fn due(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<ScheduleId>, PersistenceError> {
    due_after(conn, now, Uuid::nil(), limit).await
}

/// Returns the next bounded window of due schedules after `after`, wrapping
/// once.
///
/// Rotation by schedule identifier keeps one persistently failing schedule
/// from occupying the same batch on every pass: a failure leaves its cursor
/// unchanged, but the next pass starts beyond it and eventually reaches every
/// other due schedule.
///
/// # Errors
///
/// Returns a database error if the read fails.
pub async fn due_after(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
    after: Uuid,
    limit: i64,
) -> Result<Vec<ScheduleId>, PersistenceError> {
    let limit = limit.clamp(1, MAX_PAGE);
    let rows: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM (
             (SELECT s.id AS id, 0 AS wrap
              FROM maintenance_schedules s
              JOIN maintenance_schedule_state st
                ON st.organization_id = s.organization_id AND st.schedule_id = s.id
              WHERE s.enabled AND st.next_due_at IS NOT NULL
                AND st.next_due_at <= $2 AND s.id > $3
              ORDER BY s.id LIMIT $1)
             UNION ALL
             (SELECT s.id AS id, 1 AS wrap
              FROM maintenance_schedules s
              JOIN maintenance_schedule_state st
                ON st.organization_id = s.organization_id AND st.schedule_id = s.id
              WHERE s.enabled AND st.next_due_at IS NOT NULL
                AND st.next_due_at <= $2 AND s.id <= $3
              ORDER BY s.id LIMIT $1)
         ) candidates
         ORDER BY wrap, id LIMIT $1",
    )
    .bind(limit)
    .bind(now)
    .bind(after)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(ScheduleId::from_uuid).collect())
}

/// Arms enabled schedules that have no due state yet.
///
/// Schedules persisted before BS-019.T2 were created before the state table
/// existed. Their first scheduler pass arms each one at its first occurrence
/// after `now`, exactly as creation does. Timing that cannot be parsed or names
/// no date is stored dormant (`NULL`), so it cannot occupy every bounded scan;
/// editing the schedule re-arms it.
///
/// # Errors
///
/// Returns a database error if the scan or write fails.
pub async fn arm_missing(
    conn: &mut PgConnection,
    now: DateTime<Utc>,
    limit: i64,
) -> Result<usize, PersistenceError> {
    #[derive(sqlx::FromRow)]
    struct Unarmed {
        organization_id: Uuid,
        id: Uuid,
        cron_expression: String,
        time_zone: String,
    }

    let limit = limit.clamp(1, MAX_PAGE);
    let rows = sqlx::query_as::<_, Unarmed>(
        "SELECT s.organization_id, s.id, s.cron_expression, s.time_zone
         FROM maintenance_schedules s
         LEFT JOIN maintenance_schedule_state st
           ON st.organization_id = s.organization_id AND st.schedule_id = s.id
         WHERE s.enabled AND st.schedule_id IS NULL
         ORDER BY s.id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    let mut armed = 0;
    for row in rows {
        let next = CronSchedule::parse(&row.cron_expression, &row.time_zone)
            .ok()
            .and_then(|timing| timing.next_occurrence(now));
        sqlx::query(
            "INSERT INTO maintenance_schedule_state (organization_id, schedule_id, next_due_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (organization_id, schedule_id) DO NOTHING",
        )
        .bind(row.organization_id)
        .bind(row.id)
        .bind(next)
        .execute(&mut *conn)
        .await?;
        armed += 1;
    }
    Ok(armed)
}

/// The schedule's next due instant, if it is enabled and reachable.
///
/// # Errors
///
/// Returns a database error if the read fails.
pub async fn next_due(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    schedule_id: ScheduleId,
) -> Result<Option<DateTime<Utc>>, PersistenceError> {
    let due: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
        "SELECT next_due_at FROM maintenance_schedule_state
         WHERE organization_id = $1 AND schedule_id = $2",
    )
    .bind(organization_id.as_uuid())
    .bind(schedule_id.as_uuid())
    .fetch_optional(conn)
    .await?;
    Ok(due.flatten())
}

/// Lists a schedule's evaluations, oldest first.
///
/// # Errors
///
/// Returns a database error or a corrupt-row error.
pub async fn occurrences(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    schedule_id: ScheduleId,
    limit: i64,
) -> Result<Vec<Occurrence>, PersistenceError> {
    let limit = limit.clamp(1, MAX_PAGE);
    let rows = sqlx::query_as::<_, OccurrenceRow>(concat!(
        "SELECT ",
        occurrence_columns!(),
        " FROM maintenance_schedule_occurrences
         WHERE organization_id = $1 AND schedule_id = $2
         ORDER BY scheduled_for, id LIMIT $3"
    ))
    .bind(organization_id.as_uuid())
    .bind(schedule_id.as_uuid())
    .bind(limit)
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// Lists a schedule's evaluations newest first, before `before` when given.
///
/// `scheduled_for` is unique within a schedule (the admission identity), so it
/// is the whole cursor. The unique index also serves this read: equality on
/// the schedule, a descending range on the instant.
///
/// # Errors
///
/// Returns a database error or a corrupt-row error.
pub async fn occurrences_before(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    schedule_id: ScheduleId,
    before: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<Occurrence>, PersistenceError> {
    let limit = limit.clamp(1, MAX_PAGE);
    let rows = sqlx::query_as::<_, OccurrenceRow>(concat!(
        "SELECT ",
        occurrence_columns!(),
        " FROM maintenance_schedule_occurrences
         WHERE organization_id = $1 AND schedule_id = $2
           AND ($3::timestamptz IS NULL OR scheduled_for < $3)
         ORDER BY scheduled_for DESC
         LIMIT $4"
    ))
    .bind(organization_id.as_uuid())
    .bind(schedule_id.as_uuid())
    .bind(before)
    .bind(limit)
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// Admits the schedule's next due evaluation at the current instant.
///
/// The admission clock is sampled **after** the schedule and state rows are
/// locked, so a scheduler pass that waited on a lock cannot admit a run after
/// its maintenance window closed while the pass was blocked.
///
/// The whole decision is one transaction: leadership is fenced, the schedule
/// row is locked so a concurrent edit cannot change the timing under the
/// decision, the occurrence row is written, an overlapping table job coalesces
/// it or a new job is created under the service identity, and the next due
/// instant advances. Nothing is committed when no evaluation is due.
///
/// # Errors
///
/// Lost leadership, database, corrupt-timing, and audit errors. An ineligible
/// service identity, a disabled policy, or an instant outside the maintenance
/// window is recorded as a `Skipped` occurrence, not an error.
pub async fn admit(
    conn: &mut PgConnection,
    schedule_id: ScheduleId,
    epoch: i64,
) -> Result<Option<Occurrence>, PersistenceError> {
    admit_with_clock(conn, schedule_id, epoch, Utc::now).await
}

/// Admits the schedule's next due evaluation at `now`.
///
/// The explicit instant keeps tests and replay diagnostics deterministic. Like
/// [`admit`], it is applied after the schedule and state rows are locked.
///
/// # Errors
///
/// As [`admit`].
pub async fn admit_at(
    conn: &mut PgConnection,
    schedule_id: ScheduleId,
    now: DateTime<Utc>,
    epoch: i64,
) -> Result<Option<Occurrence>, PersistenceError> {
    admit_with_clock(conn, schedule_id, epoch, move || now).await
}

/// Admits with an injected clock sampled after the admission locks.
///
/// The clock is evaluated once `lock_due` has taken both row locks. Tests use
/// this to prove that a pass which waited on the schedule row uses the instant
/// at which it actually decides, not the instant at which the pass started.
/// Prefer [`admit`] or [`admit_at`].
///
/// # Errors
///
/// As [`admit`].
#[doc(hidden)]
pub async fn admit_with_clock<F>(
    conn: &mut PgConnection,
    schedule_id: ScheduleId,
    epoch: i64,
    clock: F,
) -> Result<Option<Occurrence>, PersistenceError>
where
    F: FnOnce() -> DateTime<Utc> + Send,
{
    admit_with_health(conn, schedule_id, epoch, clock, None).await
}

/// Admits with a health verdict for the policy's trigger.
///
/// `verdict` is the table's evaluated findings, or `None` when the table has
/// no complete sample. A policy without a trigger ignores it; a policy with
/// one admits only when a finding of the trigger's kind reaches its severity.
///
/// # Errors
///
/// As [`admit_with_clock`].
pub async fn admit_with_health<F>(
    conn: &mut PgConnection,
    schedule_id: ScheduleId,
    epoch: i64,
    clock: F,
    verdict: Option<&HealthVerdict>,
) -> Result<Option<Occurrence>, PersistenceError>
where
    F: FnOnce() -> DateTime<Utc> + Send,
{
    let mut tx = conn.begin().await?;
    crate::leadership::ensure_leadership(&mut tx, epoch).await?;
    let Some((schedule, first)) = lock_due(&mut tx, schedule_id).await? else {
        return Ok(None);
    };
    // Sample only after both locks: a pass that waited here must decide with
    // the clock it has now, not the clock it had when the pass began.
    let now = clock();
    if first > now {
        return Ok(None);
    }
    if !schedule.enabled {
        sqlx::query(
            "DELETE FROM maintenance_schedule_state
             WHERE organization_id = $1 AND schedule_id = $2",
        )
        .bind(schedule.organization_id)
        .bind(schedule.id)
        .execute(&mut *tx)
        .await?;
        crate::leadership::ensure_leadership(&mut tx, epoch).await?;
        tx.commit().await?;
        return Ok(None);
    }
    let timing =
        CronSchedule::parse(&schedule.cron_expression, &schedule.time_zone).map_err(|error| {
            PersistenceError::CorruptValue {
                entity: "schedule timing",
                value: error.to_string(),
            }
        })?;
    let until = coalesced_until(&timing, first, now);
    let next = timing.next_occurrence(now);
    let occurrence = decide(
        &mut tx, &schedule, &timing, first, until, next, now, verdict,
    )
    .await?;
    // Recheck the epoch before committing the job and its occurrence, so a
    // coordinator that lost leadership mid-decision cannot add work.
    crate::leadership::ensure_leadership(&mut tx, epoch).await?;
    tx.commit().await?;
    Ok(Some(occurrence))
}

/// Locks the schedule and returns its next due instant when it has one.
async fn lock_due(
    conn: &mut PgConnection,
    schedule_id: ScheduleId,
) -> Result<Option<(DueSchedule, DateTime<Utc>)>, PersistenceError> {
    let schedule = sqlx::query_as::<_, DueSchedule>(
        "SELECT s.id, s.organization_id, s.policy_id, s.table_identity_id,
                s.cron_expression, s.time_zone, s.window_start_minute,
                s.window_end_minute, s.service_principal_id, s.enabled,
                p.enabled AS policy_enabled, p.health_trigger
         FROM maintenance_schedules s
         JOIN maintenance_policies p
           ON p.organization_id = s.organization_id AND p.id = s.policy_id
         WHERE s.id = $1
         FOR UPDATE OF s",
    )
    .bind(schedule_id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    let Some(schedule) = schedule else {
        return Ok(None);
    };
    let due: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
        "SELECT next_due_at FROM maintenance_schedule_state
         WHERE organization_id = $1 AND schedule_id = $2
         FOR UPDATE",
    )
    .bind(schedule.organization_id)
    .bind(schedule.id)
    .fetch_optional(&mut *conn)
    .await?;
    // A state row is deleted on disable, so a missing row is already "not due".
    let Some(Some(first)) = due else {
        return Ok(None);
    };
    Ok(Some((schedule, first)))
}

/// Applies one due evaluation: skip, coalesce, or create the job.
///
/// The maintenance window is checked against `now`, the instant at which the
/// job would start, not against the nominal occurrence time. A missed
/// occurrence that is evaluated after its window closed is skipped rather than
/// starting maintenance during active writes; the next recurrence can admit
/// again.
#[expect(
    clippy::too_many_arguments,
    reason = "one decision already carries the locked schedule, timing, and span"
)]
async fn decide(
    conn: &mut PgConnection,
    schedule: &DueSchedule,
    timing: &CronSchedule,
    first: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
    next: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    verdict: Option<&HealthVerdict>,
) -> Result<Occurrence, PersistenceError> {
    if !schedule.policy_enabled {
        return skip(
            conn,
            schedule,
            first,
            until,
            next,
            SkipReason::PolicyDisabled,
        )
        .await;
    }
    if let Err(error) = schedules::require_execution_identity(
        conn,
        OrganizationId::from_uuid(schedule.organization_id),
        PrincipalId::from_uuid(schedule.service_principal_id),
        TableId::from_uuid(schedule.table_identity_id),
    )
    .await
    {
        return match error {
            PersistenceError::IneligibleServiceIdentity { .. } => {
                skip(
                    conn,
                    schedule,
                    first,
                    until,
                    next,
                    SkipReason::ServiceIdentityIneligible,
                )
                .await
            }
            error => Err(error),
        };
    }
    match crate::tenancy::lock_execution_route(
        conn,
        OrganizationId::from_uuid(schedule.organization_id),
        PrincipalId::from_uuid(schedule.service_principal_id),
        TableId::from_uuid(schedule.table_identity_id),
    )
    .await?
    {
        ExecutionRoute::Allowed => {}
        ExecutionRoute::MaintenancePaused => {
            return skip(
                conn,
                schedule,
                first,
                until,
                next,
                SkipReason::MaintenancePaused,
            )
            .await;
        }
        ExecutionRoute::ConnectionDisabled => {
            return skip(
                conn,
                schedule,
                first,
                until,
                next,
                SkipReason::ConnectionDisabled,
            )
            .await;
        }
    }
    let window =
        schedules::window_from_row(schedule.window_start_minute, schedule.window_end_minute)?;
    if let Some(window) = window
        && !window.contains_instant(now, timing.time_zone.zone())
    {
        return skip(
            conn,
            schedule,
            first,
            until,
            next_in_window(window, timing, now),
            SkipReason::OutsideWindow,
        )
        .await;
    }
    match trigger_gate(schedule, verdict)? {
        TriggerGate::Unmet => {
            skip(
                conn,
                schedule,
                first,
                until,
                next,
                SkipReason::HealthTriggerNotMet,
            )
            .await
        }
        TriggerGate::Proceed(trigger) => {
            admit_or_coalesce(conn, schedule, first, until, next, trigger).await
        }
    }
}

/// The first occurrence at or after the window's next opening.
///
/// One skip covers the whole out-of-window span instead of recording one row
/// per cron tick. A window opening always exists for a valid schedule, but a
/// corrupt time-zone transition leaves the schedule dormant.
fn next_in_window(
    window: MaintenanceWindow,
    timing: &CronSchedule,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    window
        .next_opening(now, timing.time_zone.zone())
        .and_then(|opening| timing.next_occurrence(opening - chrono::Duration::seconds(1)))
}

/// Whether admission proceeds, and the condition that admits it.
enum TriggerGate {
    /// Proceed, naming the admitting condition when a trigger caused it.
    Proceed(Option<HealthTrigger>),
    /// The policy's condition is not satisfied.
    Unmet,
}

/// Checks the policy's health condition against the table's verdict.
fn trigger_gate(
    schedule: &DueSchedule,
    verdict: Option<&HealthVerdict>,
) -> Result<TriggerGate, PersistenceError> {
    let trigger: Option<HealthTrigger> = schedule
        .health_trigger
        .clone()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| PersistenceError::CorruptValue {
            entity: "policy health trigger",
            value: error.to_string(),
        })?;
    match trigger {
        None => Ok(TriggerGate::Proceed(None)),
        Some(trigger) if verdict.is_some_and(|verdict| verdict.reaches(&trigger)) => {
            Ok(TriggerGate::Proceed(Some(trigger)))
        }
        Some(_) => Ok(TriggerGate::Unmet),
    }
}

/// Records a permitted evaluation and its job or existing table owner.
async fn admit_or_coalesce(
    conn: &mut PgConnection,
    schedule: &DueSchedule,
    first: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
    next: Option<DateTime<Utc>>,
    trigger: Option<HealthTrigger>,
) -> Result<Occurrence, PersistenceError> {
    let occurrence = insert_pending(conn, schedule, first, until).await?;
    let blocking: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM jobs
         WHERE organization_id = $1 AND table_identity_id = $2
           AND state NOT IN ('Succeeded','NoOp','Cancelled','ReplanRequired','Failed')
         ORDER BY created_at, id LIMIT 1",
    )
    .bind(schedule.organization_id)
    .bind(schedule.table_identity_id)
    .fetch_optional(&mut *conn)
    .await?;
    let occurrence = if let Some(blocking) = blocking {
        finish(
            conn,
            occurrence.id,
            "Coalesced",
            Some(blocking),
            Some("TableBusy"),
        )
        .await?
    } else {
        let job = jobs::create_job_with_trigger(
            conn,
            &NewJob {
                id: JobId::generate(),
                organization_id: OrganizationId::from_uuid(schedule.organization_id),
                table_id: TableId::from_uuid(schedule.table_identity_id),
                retry_of: None,
                created_by: PrincipalId::from_uuid(schedule.service_principal_id),
            },
            trigger,
        )
        .await?;
        let occurrence = finish(
            conn,
            occurrence.id,
            "Admitted",
            Some(*job.id.as_uuid()),
            None,
        )
        .await?;
        record_admitted(conn, schedule, &occurrence).await?;
        occurrence
    };
    advance(conn, schedule, next).await?;
    Ok(occurrence)
}

/// The health condition a schedule's policy declares, with its thresholds.
#[derive(Clone, Debug, PartialEq)]
pub struct ScheduleTrigger {
    /// The owning organization.
    pub organization_id: OrganizationId,
    /// The policy that declares the trigger.
    pub policy_id: PolicyId,
    /// The table the schedule maintains.
    pub table_id: TableId,
    /// The thresholds the trigger evaluates with.
    pub thresholds: HealthThresholds,
    /// The condition admission must satisfy.
    pub trigger: HealthTrigger,
}

/// Reads the health condition of `schedule_id`, when its policy declares one.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the read fails, and
/// [`PersistenceError::CorruptValue`] when a stored threshold or trigger
/// cannot be parsed.
pub async fn schedule_trigger(
    conn: &mut PgConnection,
    schedule_id: ScheduleId,
) -> Result<Option<ScheduleTrigger>, PersistenceError> {
    let row: Option<(Uuid, Uuid, Uuid, Value, Value)> = sqlx::query_as(
        "SELECT s.organization_id, s.policy_id, s.table_identity_id, \
                p.health_thresholds, p.health_trigger \
         FROM maintenance_schedules s \
         JOIN maintenance_policies p \
           ON p.organization_id = s.organization_id AND p.id = s.policy_id \
         WHERE s.id = $1 AND p.health_trigger IS NOT NULL",
    )
    .bind(schedule_id.as_uuid())
    .fetch_optional(conn)
    .await?;
    let Some((organization_id, policy_id, table_id, thresholds, trigger)) = row else {
        return Ok(None);
    };
    let thresholds: HealthThresholds =
        serde_json::from_value(thresholds).map_err(|error| PersistenceError::CorruptValue {
            entity: "policy health thresholds",
            value: error.to_string(),
        })?;
    thresholds
        .validate()
        .map_err(|error| PersistenceError::CorruptValue {
            entity: "policy health thresholds",
            value: error.to_string(),
        })?;
    let trigger: HealthTrigger =
        serde_json::from_value(trigger).map_err(|error| PersistenceError::CorruptValue {
            entity: "policy health trigger",
            value: error.to_string(),
        })?;
    Ok(Some(ScheduleTrigger {
        organization_id: OrganizationId::from_uuid(organization_id),
        policy_id: PolicyId::from_uuid(policy_id),
        table_id: TableId::from_uuid(table_id),
        thresholds,
        trigger,
    }))
}

/// Whether an admitted occurrence for `job` belongs to a disabled policy.
///
/// Publication rechecks this so disabling a policy stops a job before it sends
/// a commit. Disabling only the schedule does not affect an existing job: that
/// stops future admission, while cancelling a running job remains explicit.
///
/// # Errors
///
/// Returns a database error if the read fails.
pub async fn job_policy_disabled(
    conn: &mut PgConnection,
    job: JobId,
) -> Result<bool, PersistenceError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
             FROM maintenance_schedule_occurrences o
             JOIN maintenance_schedules s
               ON s.organization_id = o.organization_id AND s.id = o.schedule_id
             JOIN maintenance_policies p
               ON p.organization_id = s.organization_id AND p.id = s.policy_id
             WHERE o.job_id = $1 AND o.state = 'Admitted' AND NOT p.enabled
         )",
    )
    .bind(job.as_uuid())
    .fetch_one(conn)
    .await?)
}

/// The last instant folded into an evaluation, when more than one was due.
///
/// One backward search replaces resolving every missed instant in turn, so a
/// minute-frequency schedule does not spend seconds inside the admission
/// transaction after a long outage.
fn coalesced_until(
    timing: &CronSchedule,
    first: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    timing
        .previous_occurrence(now)
        .filter(|latest| *latest > first)
}

async fn insert_pending(
    conn: &mut PgConnection,
    schedule: &DueSchedule,
    first: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
) -> Result<Occurrence, PersistenceError> {
    let row = sqlx::query_as::<_, OccurrenceRow>(concat!(
        "INSERT INTO maintenance_schedule_occurrences
             (id, organization_id, schedule_id, scheduled_for, coalesced_until, state)
         VALUES ($1, $2, $3, $4, $5, 'Pending')
         RETURNING ",
        occurrence_columns!()
    ))
    .bind(Uuid::now_v7())
    .bind(schedule.organization_id)
    .bind(schedule.id)
    .bind(first)
    .bind(until)
    .fetch_one(conn)
    .await
    .map_err(|error| PersistenceError::from_write("schedule occurrence", error))?;
    row.try_into()
}

async fn finish(
    conn: &mut PgConnection,
    occurrence: Uuid,
    state: &'static str,
    job: Option<Uuid>,
    reason: Option<&'static str>,
) -> Result<Occurrence, PersistenceError> {
    let row = sqlx::query_as::<_, OccurrenceRow>(concat!(
        "UPDATE maintenance_schedule_occurrences
         SET state = $2, job_id = $3, reason = $4
         WHERE id = $1
         RETURNING ",
        occurrence_columns!()
    ))
    .bind(occurrence)
    .bind(state)
    .bind(job)
    .bind(reason)
    .fetch_one(conn)
    .await
    .map_err(|error| PersistenceError::from_write("schedule occurrence", error))?;
    row.try_into()
}

async fn skip(
    conn: &mut PgConnection,
    schedule: &DueSchedule,
    first: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
    next: Option<DateTime<Utc>>,
    reason: SkipReason,
) -> Result<Occurrence, PersistenceError> {
    // A skip is final before the row exists, so it is written in one statement
    // rather than inserted as `Pending` and updated. The row is still the
    // durable identity that prevents a replay from admitting a second job.
    let row = sqlx::query_as::<_, OccurrenceRow>(concat!(
        "INSERT INTO maintenance_schedule_occurrences
             (id, organization_id, schedule_id, scheduled_for, coalesced_until,
              state, reason)
         VALUES ($1, $2, $3, $4, $5, 'Skipped', $6)
         RETURNING ",
        occurrence_columns!()
    ))
    .bind(Uuid::now_v7())
    .bind(schedule.organization_id)
    .bind(schedule.id)
    .bind(first)
    .bind(until)
    .bind(reason.as_str())
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write("schedule occurrence", error))?;
    let occurrence: Occurrence = row.try_into()?;
    if reason.audited() {
        audit::record(
            conn,
            &NewAuditEvent {
                organization_id: Some(OrganizationId::from_uuid(schedule.organization_id)),
                actor_principal_id: Some(PrincipalId::from_uuid(schedule.service_principal_id)),
                actor_kind: ActorKind::Service,
                action: AuditAction::JobAdmissionDenied.as_str(),
                resource_type: "schedule",
                resource_id: Some(&ScheduleId::from_uuid(schedule.id).to_string()),
                outcome: AuditOutcome::Denied,
                detail: json!({
                    "table_id": TableId::from_uuid(schedule.table_identity_id),
                    "scheduled_for": first.to_rfc3339(),
                    "reason": reason.as_str(),
                }),
            },
        )
        .await?;
    }
    advance(conn, schedule, next).await?;
    Ok(occurrence)
}

async fn record_admitted(
    conn: &mut PgConnection,
    schedule: &DueSchedule,
    occurrence: &Occurrence,
) -> Result<(), PersistenceError> {
    let job = occurrence.job_id.ok_or(PersistenceError::CorruptValue {
        entity: "schedule occurrence",
        value: "admitted without a job".to_owned(),
    })?;
    audit::record(
        conn,
        &NewAuditEvent {
            organization_id: Some(OrganizationId::from_uuid(schedule.organization_id)),
            actor_principal_id: Some(PrincipalId::from_uuid(schedule.service_principal_id)),
            actor_kind: ActorKind::Service,
            action: AuditAction::JobSubmitted.as_str(),
            resource_type: "job",
            resource_id: Some(&job.to_string()),
            outcome: AuditOutcome::Allowed,
            detail: json!({
                "schedule_id": ScheduleId::from_uuid(schedule.id),
                "policy_id": PolicyId::from_uuid(schedule.policy_id),
                "table_id": TableId::from_uuid(schedule.table_identity_id),
                "scheduled_for": occurrence.scheduled_for.to_rfc3339(),
                "coalesced_until": occurrence.coalesced_until.map(|until| until.to_rfc3339()),
            }),
        },
    )
    .await?;
    Ok(())
}

async fn advance(
    conn: &mut PgConnection,
    schedule: &DueSchedule,
    next: Option<DateTime<Utc>>,
) -> Result<(), PersistenceError> {
    sqlx::query(
        "UPDATE maintenance_schedule_state
         SET next_due_at = $3, updated_at = clock_timestamp()
         WHERE organization_id = $1 AND schedule_id = $2",
    )
    .bind(schedule.organization_id)
    .bind(schedule.id)
    .bind(next)
    .execute(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occurrence_states_round_trip_through_their_persisted_names() {
        for state in [
            OccurrenceState::Pending,
            OccurrenceState::Admitted,
            OccurrenceState::Coalesced,
            OccurrenceState::Skipped,
        ] {
            assert_eq!(
                OccurrenceState::parse(state.as_str()).expect("a stored state decodes"),
                state
            );
        }
        assert!(matches!(
            OccurrenceState::parse("Unknown"),
            Err(PersistenceError::CorruptValue { .. })
        ));
    }
}
