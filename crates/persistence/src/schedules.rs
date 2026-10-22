//! Maintenance policies and schedules (BS-019.T1).
//!
//! A policy holds what to run; a schedule holds when, where, and under which
//! service identity. Both are organization-scoped configuration rows with an
//! optimistic `revision`: an update names the revision it read, so two editors
//! cannot silently overwrite each other. Disabling is an ordinary update that
//! flips `enabled`, which keeps the row and its audit history.
//!
//! Cron syntax and the time-zone name are parsed by
//! [`bergsmith_common::schedule`] before they reach this module, so an invalid
//! timing contract cannot be persisted. A schedule may also name a daily
//! local-time window, and a policy names the most input one run may rewrite
//! (BS-019.T3). Schedule creation additionally refuses a service principal
//! that is not a service account or that holds no grant covering
//! `jobs:submit` for the table.

use bergsmith_common::ids::{JobId, OrganizationId, PolicyId, PrincipalId, ScheduleId, TableId};
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_common::policy::{MaintenanceOperation, PolicySpec};
use bergsmith_common::schedule::CronSchedule;
use bergsmith_common::window::MaintenanceWindow;
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::tenancy::PrincipalKind;
use crate::{PersistenceError, grants, tenancy};

/// A reusable maintenance policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintenancePolicy {
    /// Identifier.
    pub id: PolicyId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Unique name within the organization.
    pub name: String,
    /// Operation and bounded parameters.
    pub specification: PolicySpec,
    /// Whether schedules may use it and admission may run it.
    pub enabled: bool,
    /// Optimistic concurrency revision.
    pub revision: i64,
    /// Who created it.
    pub created_by: PrincipalId,
    /// When it was created (database time).
    pub created_at: DateTime<Utc>,
    /// When it was last changed (database time).
    pub updated_at: DateTime<Utc>,
}

/// Fields for a new policy.
#[derive(Clone, Debug)]
pub struct NewMaintenancePolicy {
    /// Identifier to assign.
    pub id: PolicyId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Unique name within the organization.
    pub name: String,
    /// Operation and bounded parameters.
    pub specification: PolicySpec,
    /// Who is creating it.
    pub created_by: PrincipalId,
}

/// A policy update, applied only when the stored revision matches.
#[derive(Clone, Debug)]
pub struct PolicyUpdate {
    /// New name, if it changes.
    pub name: Option<String>,
    /// Complete replacement specification, if any parameter changes.
    pub specification: Option<PolicySpec>,
    /// Whether the policy is enabled, if that changes.
    pub enabled: Option<bool>,
    /// The revision the caller read.
    pub expected_revision: i64,
}

/// A schedule that runs one policy against one table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintenanceSchedule {
    /// Identifier.
    pub id: ScheduleId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// The policy this schedule runs.
    pub policy_id: PolicyId,
    /// The canonical table it runs against.
    pub table_id: TableId,
    /// When it may start, in its local time zone.
    pub timing: CronSchedule,
    /// The local time-of-day range in which a run may start, if one is set.
    pub window: Option<MaintenanceWindow>,
    /// The service identity it executes as.
    pub service_principal_id: PrincipalId,
    /// Whether it may admit new occurrences.
    pub enabled: bool,
    /// Optimistic concurrency revision.
    pub revision: i64,
    /// Who created it.
    pub created_by: PrincipalId,
    /// When it was created (database time).
    pub created_at: DateTime<Utc>,
    /// When it was last changed (database time).
    pub updated_at: DateTime<Utc>,
}

/// Fields for a new schedule.
#[derive(Clone, Debug)]
pub struct NewMaintenanceSchedule {
    /// Identifier to assign.
    pub id: ScheduleId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// The policy it runs.
    pub policy_id: PolicyId,
    /// The canonical table it runs against.
    pub table_id: TableId,
    /// When it may start.
    pub timing: CronSchedule,
    /// The local time-of-day range in which a run may start, if one is set.
    pub window: Option<MaintenanceWindow>,
    /// The service identity it executes as.
    pub service_principal_id: PrincipalId,
    /// Who is creating it.
    pub created_by: PrincipalId,
}

/// A schedule update, applied only when the stored revision matches.
#[derive(Clone, Debug)]
pub struct ScheduleUpdate {
    /// Complete replacement timing, if the expression or zone changes.
    pub timing: Option<CronSchedule>,
    /// The window change: `None` leaves the stored window alone, `Some(None)`
    /// clears it, and `Some(Some(window))` replaces it.
    pub window: Option<Option<MaintenanceWindow>>,
    /// Whether the schedule is enabled, if that changes.
    pub enabled: Option<bool>,
    /// The revision the caller read.
    pub expected_revision: i64,
}

#[derive(sqlx::FromRow)]
struct PolicyRow {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    operation: String,
    target_file_size_bytes: i64,
    min_input_files: i32,
    max_input_bytes: i64,
    enabled: bool,
    revision: i64,
    created_by: Uuid,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<PolicyRow> for MaintenancePolicy {
    type Error = PersistenceError;

    fn try_from(row: PolicyRow) -> Result<Self, Self::Error> {
        let operation = MaintenanceOperation::from_name(&row.operation).ok_or_else(|| {
            PersistenceError::CorruptValue {
                entity: "maintenance operation",
                value: row.operation.clone(),
            }
        })?;
        let target_file_size_bytes = u64::try_from(row.target_file_size_bytes).map_err(|_| {
            PersistenceError::CorruptValue {
                entity: "target file size",
                value: row.target_file_size_bytes.to_string(),
            }
        })?;
        let min_input_files =
            u32::try_from(row.min_input_files).map_err(|_| PersistenceError::CorruptValue {
                entity: "minimum input files",
                value: row.min_input_files.to_string(),
            })?;
        let max_input_bytes =
            u64::try_from(row.max_input_bytes).map_err(|_| PersistenceError::CorruptValue {
                entity: "maximum input bytes",
                value: row.max_input_bytes.to_string(),
            })?;
        let specification = PolicySpec::new(
            operation,
            target_file_size_bytes,
            min_input_files,
            max_input_bytes,
        )
        .map_err(|error| PersistenceError::CorruptValue {
            entity: "policy specification",
            value: error.to_string(),
        })?;
        Ok(Self {
            id: PolicyId::from_uuid(row.id),
            organization_id: OrganizationId::from_uuid(row.organization_id),
            name: row.name,
            specification,
            enabled: row.enabled,
            revision: row.revision,
            created_by: PrincipalId::from_uuid(row.created_by),
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct ScheduleRow {
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
    revision: i64,
    created_by: Uuid,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<ScheduleRow> for MaintenanceSchedule {
    type Error = PersistenceError;

    fn try_from(row: ScheduleRow) -> Result<Self, Self::Error> {
        let timing =
            CronSchedule::parse(&row.cron_expression, &row.time_zone).map_err(|error| {
                PersistenceError::CorruptValue {
                    entity: "schedule timing",
                    value: error.to_string(),
                }
            })?;
        let window = window_from_row(row.window_start_minute, row.window_end_minute)?;
        Ok(Self {
            id: ScheduleId::from_uuid(row.id),
            organization_id: OrganizationId::from_uuid(row.organization_id),
            policy_id: PolicyId::from_uuid(row.policy_id),
            table_id: TableId::from_uuid(row.table_identity_id),
            timing,
            window,
            service_principal_id: PrincipalId::from_uuid(row.service_principal_id),
            enabled: row.enabled,
            revision: row.revision,
            created_by: PrincipalId::from_uuid(row.created_by),
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

/// Columns selected for [`MaintenancePolicy`], in [`PolicyRow`] order.
macro_rules! policy_columns {
    () => {
        "id, organization_id, name, operation, target_file_size_bytes, \
         min_input_files, max_input_bytes, enabled, revision, created_by, \
         created_at, updated_at"
    };
}

/// Columns selected for [`MaintenanceSchedule`], in [`ScheduleRow`] order.
macro_rules! schedule_columns {
    () => {
        "id, organization_id, policy_id, table_identity_id, cron_expression, \
         time_zone, window_start_minute, window_end_minute, \
         service_principal_id, enabled, revision, created_by, created_at, \
         updated_at"
    };
}

/// Rebuilds a schedule's window from its two nullable columns.
pub(crate) fn window_from_row(
    start: Option<i32>,
    end: Option<i32>,
) -> Result<Option<MaintenanceWindow>, PersistenceError> {
    MaintenanceWindow::from_minutes(start, end).map_err(|error| PersistenceError::CorruptValue {
        entity: "maintenance window",
        value: error.to_string(),
    })
}

/// The most rows one list call returns. Callers may ask for one extra row to
/// detect whether a next page exists.
const MAX_PAGE: i64 = 101;

/// Creates a policy.
///
/// # Errors
///
/// [`PersistenceError::AlreadyExists`] when the organization already has a
/// policy with this name, [`PersistenceError::MissingReference`] when the
/// creator is not in the organization, and database errors.
pub async fn create_policy(
    conn: &mut PgConnection,
    new: &NewMaintenancePolicy,
) -> Result<MaintenancePolicy, PersistenceError> {
    let row = sqlx::query_as::<_, PolicyRow>(concat!(
        "INSERT INTO maintenance_policies
             (id, organization_id, name, operation, target_file_size_bytes,
              min_input_files, max_input_bytes, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING ",
        policy_columns!()
    ))
    .bind(new.id.as_uuid())
    .bind(new.organization_id.as_uuid())
    .bind(&new.name)
    .bind(new.specification.operation.as_str())
    .bind(
        i64::try_from(new.specification.target_file_size_bytes).map_err(|_| {
            PersistenceError::InvalidValue {
                entity: "maintenance policy",
                constraint: "target_file_size_bytes".to_owned(),
            }
        })?,
    )
    .bind(
        i32::try_from(new.specification.min_input_files).map_err(|_| {
            PersistenceError::InvalidValue {
                entity: "maintenance policy",
                constraint: "min_input_files".to_owned(),
            }
        })?,
    )
    .bind(
        i64::try_from(new.specification.max_input_bytes).map_err(|_| {
            PersistenceError::InvalidValue {
                entity: "maintenance policy",
                constraint: "max_input_bytes".to_owned(),
            }
        })?,
    )
    .bind(new.created_by.as_uuid())
    .fetch_one(conn)
    .await
    .map_err(|error| PersistenceError::from_write("maintenance policy", error))?;
    row.try_into()
}

/// Reads one policy.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the organization has no such policy,
/// and database errors.
pub async fn policy(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: PolicyId,
) -> Result<MaintenancePolicy, PersistenceError> {
    let row = sqlx::query_as::<_, PolicyRow>(concat!(
        "SELECT ",
        policy_columns!(),
        " FROM maintenance_policies WHERE organization_id = $1 AND id = $2"
    ))
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "maintenance policy",
        id: id.to_string(),
    })?;
    row.try_into()
}

/// The policy that admitted `job`, when the job came from a schedule.
///
/// A scheduled job is planned with its policy's target size, minimum input
/// count, and per-run budget (ADR 0039/0040). A manual job has no row here and
/// keeps the base planner configuration.
///
/// # Errors
///
/// Returns a database error if the read fails.
pub async fn policy_for_job(
    conn: &mut PgConnection,
    job: JobId,
) -> Result<Option<MaintenancePolicy>, PersistenceError> {
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT o.organization_id, s.policy_id
         FROM maintenance_schedule_occurrences o
         JOIN maintenance_schedules s
           ON s.organization_id = o.organization_id AND s.id = o.schedule_id
         WHERE o.job_id = $1 AND o.state = 'Admitted'",
    )
    .bind(job.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    let Some((organization, policy_id)) = row else {
        return Ok(None);
    };
    policy(
        conn,
        OrganizationId::from_uuid(organization),
        PolicyId::from_uuid(policy_id),
    )
    .await
    .map(Some)
}

/// Lists an organization's policies in identifier order.
///
/// `after` is the last identifier the caller saw; `limit` is capped at 101 so one extra row can prove a next page.
///
/// # Errors
///
/// Returns a database error if the query fails.
pub async fn policies(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    after: Option<PolicyId>,
    limit: i64,
) -> Result<Vec<MaintenancePolicy>, PersistenceError> {
    let limit = limit.clamp(1, MAX_PAGE);
    let rows = if let Some(after) = after {
        sqlx::query_as::<_, PolicyRow>(concat!(
            "SELECT ",
            policy_columns!(),
            " FROM maintenance_policies
             WHERE organization_id = $1 AND id > $2 ORDER BY id LIMIT $3"
        ))
        .bind(organization_id.as_uuid())
        .bind(after.as_uuid())
        .bind(limit)
        .fetch_all(conn)
        .await?
    } else {
        sqlx::query_as::<_, PolicyRow>(concat!(
            "SELECT ",
            policy_columns!(),
            " FROM maintenance_policies
             WHERE organization_id = $1 ORDER BY id LIMIT $2"
        ))
        .bind(organization_id.as_uuid())
        .bind(limit)
        .fetch_all(conn)
        .await?
    };
    rows.into_iter().map(TryInto::try_into).collect()
}

/// Applies a policy update when the stored revision still matches.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the policy does not exist,
/// [`PersistenceError::ConfigRevisionConflict`] when another writer changed it,
/// [`PersistenceError::AlreadyExists`] for a duplicate name, and database
/// errors.
pub async fn update_policy(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: PolicyId,
    update: &PolicyUpdate,
) -> Result<MaintenancePolicy, PersistenceError> {
    let spec = update.specification;
    let row = sqlx::query_as::<_, PolicyRow>(concat!(
        "UPDATE maintenance_policies
         SET name = COALESCE($3, name),
             operation = COALESCE($4, operation),
             target_file_size_bytes = COALESCE($5, target_file_size_bytes),
             min_input_files = COALESCE($6, min_input_files),
             max_input_bytes = COALESCE($7, max_input_bytes),
             enabled = COALESCE($8, enabled),
             revision = revision + 1,
             updated_at = clock_timestamp()
         WHERE organization_id = $1 AND id = $2 AND revision = $9
         RETURNING ",
        policy_columns!()
    ))
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .bind(update.name.as_deref())
    .bind(spec.map(|spec| spec.operation.as_str()))
    .bind(
        spec.map(|spec| {
            i64::try_from(spec.target_file_size_bytes).map_err(|_| PersistenceError::InvalidValue {
                entity: "maintenance policy",
                constraint: "target_file_size_bytes".to_owned(),
            })
        })
        .transpose()?,
    )
    .bind(
        spec.map(|spec| {
            i32::try_from(spec.min_input_files).map_err(|_| PersistenceError::InvalidValue {
                entity: "maintenance policy",
                constraint: "min_input_files".to_owned(),
            })
        })
        .transpose()?,
    )
    .bind(
        spec.map(|spec| {
            i64::try_from(spec.max_input_bytes).map_err(|_| PersistenceError::InvalidValue {
                entity: "maintenance policy",
                constraint: "max_input_bytes".to_owned(),
            })
        })
        .transpose()?,
    )
    .bind(update.enabled)
    .bind(update.expected_revision)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write("maintenance policy", error))?;
    match row {
        Some(row) => row.try_into(),
        None => Err(revision_or_missing(
            conn,
            "SELECT revision FROM maintenance_policies WHERE organization_id = $1 AND id = $2",
            organization_id,
            *id.as_uuid(),
            update.expected_revision,
            "maintenance policy",
            id.to_string(),
        )
        .await?),
    }
}

/// Creates a schedule, validates its service identity, and arms its due state.
///
/// The principal must be a service account and must currently hold a grant
/// covering `jobs:submit` for the table. Admission and publication recheck that
/// grant in BS-019.T2, so revoking it later stops new work.
///
/// The schedule starts at its next occurrence after creation; creating it does
/// not run it immediately.
///
/// # Errors
///
/// [`PersistenceError::IneligibleServiceIdentity`] when the principal cannot
/// execute maintenance for the table, [`PersistenceError::AlreadyExists`] when
/// the policy already has a schedule for the table,
/// [`PersistenceError::MissingReference`] for a policy or table outside the
/// organization, and database errors.
pub async fn create_schedule(
    conn: &mut PgConnection,
    new: &NewMaintenanceSchedule,
) -> Result<MaintenanceSchedule, PersistenceError> {
    let mut tx = conn.begin().await?;
    require_execution_identity(
        &mut tx,
        new.organization_id,
        new.service_principal_id,
        new.table_id,
    )
    .await?;
    let row = sqlx::query_as::<_, ScheduleRow>(concat!(
        "INSERT INTO maintenance_schedules
             (id, organization_id, policy_id, table_identity_id, cron_expression,
              time_zone, window_start_minute, window_end_minute,
              service_principal_id, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         RETURNING ",
        schedule_columns!()
    ))
    .bind(new.id.as_uuid())
    .bind(new.organization_id.as_uuid())
    .bind(new.policy_id.as_uuid())
    .bind(new.table_id.as_uuid())
    .bind(new.timing.expression.as_str())
    .bind(new.timing.time_zone.as_str())
    .bind(new.window.map(|window| i32::from(window.start_minute())))
    .bind(new.window.map(|window| i32::from(window.end_minute())))
    .bind(new.service_principal_id.as_uuid())
    .bind(new.created_by.as_uuid())
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("maintenance schedule", error))?;
    let schedule: MaintenanceSchedule = row.try_into()?;
    let now = database_now(&mut tx).await?;
    arm_state(&mut tx, &schedule, now).await?;
    tx.commit().await?;
    Ok(schedule)
}

/// Reads one schedule.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the organization has no such schedule,
/// and database errors.
pub async fn schedule(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: ScheduleId,
) -> Result<MaintenanceSchedule, PersistenceError> {
    let row = sqlx::query_as::<_, ScheduleRow>(concat!(
        "SELECT ",
        schedule_columns!(),
        " FROM maintenance_schedules WHERE organization_id = $1 AND id = $2"
    ))
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "maintenance schedule",
        id: id.to_string(),
    })?;
    row.try_into()
}

/// Lists an organization's schedules in identifier order.
///
/// `after` is the last identifier the caller saw; `limit` is capped at 101 so one extra row can prove a next page.
///
/// # Errors
///
/// Returns a database error if the query fails.
pub async fn schedules(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    after: Option<ScheduleId>,
    limit: i64,
) -> Result<Vec<MaintenanceSchedule>, PersistenceError> {
    let limit = limit.clamp(1, MAX_PAGE);
    let rows = if let Some(after) = after {
        sqlx::query_as::<_, ScheduleRow>(concat!(
            "SELECT ",
            schedule_columns!(),
            " FROM maintenance_schedules
             WHERE organization_id = $1 AND id > $2 ORDER BY id LIMIT $3"
        ))
        .bind(organization_id.as_uuid())
        .bind(after.as_uuid())
        .bind(limit)
        .fetch_all(conn)
        .await?
    } else {
        sqlx::query_as::<_, ScheduleRow>(concat!(
            "SELECT ",
            schedule_columns!(),
            " FROM maintenance_schedules
             WHERE organization_id = $1 ORDER BY id LIMIT $2"
        ))
        .bind(organization_id.as_uuid())
        .bind(limit)
        .fetch_all(conn)
        .await?
    };
    rows.into_iter().map(TryInto::try_into).collect()
}

/// Applies a schedule update when the stored revision still matches.
///
/// The policy and table cannot change: a schedule for another pair is a new
/// row, so occurrence identity never moves between tables. Changing the timing
/// or enabling a disabled schedule re-arms it from the current instant;
/// disabling it removes the due state, so the disabled interval never
/// backfills. A redundant enable request for an already enabled schedule keeps
/// its due state, so overdue work is not silently discarded.
///
/// A window change recomputes a **future** cursor, because a skip under the old
/// window may have advanced it past runs the new window allows. An overdue
/// cursor is left alone so a due run is still evaluated under the new window.
/// The clock is sampled **after** the schedule row is locked, and the next
/// cursor is after the latest evaluated occurrence, so neither a waiting update
/// nor a clock skew can rewind onto an occurrence that admission already
/// recorded.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the schedule does not exist,
/// [`PersistenceError::ConfigRevisionConflict`] when another writer changed it, and
/// database errors.
pub async fn update_schedule(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: ScheduleId,
    update: &ScheduleUpdate,
) -> Result<MaintenanceSchedule, PersistenceError> {
    update_schedule_with_clock(conn, organization_id, id, update, UpdateClock::Database).await
}

/// Applies a schedule update at `now`.
///
/// The explicit instant keeps tests deterministic, as
/// [`crate::occurrences::admit_at`] does for admission.
///
/// # Errors
///
/// As [`update_schedule`].
pub async fn update_schedule_at(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: ScheduleId,
    update: &ScheduleUpdate,
    now: DateTime<Utc>,
) -> Result<MaintenanceSchedule, PersistenceError> {
    update_schedule_with_clock(conn, organization_id, id, update, UpdateClock::Fixed(now)).await
}

/// Applies a schedule update with a shared clock read after the schedule lock.
///
/// Tests use this to prove that a blocked update cannot rewind a cursor past an
/// occurrence that a concurrent admission already recorded. The clock is read
/// only after the schedule row is locked. Prefer [`update_schedule`] or
/// [`update_schedule_at`].
///
/// # Errors
///
/// As [`update_schedule`].
#[doc(hidden)]
pub async fn update_schedule_with_shared_clock(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: ScheduleId,
    update: &ScheduleUpdate,
    clock: &std::sync::Mutex<DateTime<Utc>>,
) -> Result<MaintenanceSchedule, PersistenceError> {
    update_schedule_with_clock(
        conn,
        organization_id,
        id,
        update,
        UpdateClock::Shared(clock),
    )
    .await
}

/// Where an update samples its clock. Every variant is read after the schedule
/// row lock, so the decision instant cannot predate a concurrent admission.
enum UpdateClock<'a> {
    /// Production: the database clock.
    Database,
    /// Tests: a fixed instant.
    Fixed(DateTime<Utc>),
    /// Tests: an instant the test can advance while the update is blocked.
    Shared(&'a std::sync::Mutex<DateTime<Utc>>),
}

impl UpdateClock<'_> {
    async fn now(&self, conn: &mut PgConnection) -> Result<DateTime<Utc>, PersistenceError> {
        match self {
            Self::Database => database_now(conn).await,
            Self::Fixed(now) => Ok(*now),
            Self::Shared(clock) => Ok(*clock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)),
        }
    }
}

async fn update_schedule_with_clock(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: ScheduleId,
    update: &ScheduleUpdate,
    clock: UpdateClock<'_>,
) -> Result<MaintenanceSchedule, PersistenceError> {
    let mut tx = conn.begin().await?;
    // Lock first: the previous enabled value decides whether this update is a
    // real re-enable or a no-op that must preserve an overdue cursor, and the
    // previous window tells us whether this update actually changes it.
    let previous: Option<(bool, i64, Option<i32>, Option<i32>)> = sqlx::query_as(
        "SELECT enabled, revision, window_start_minute, window_end_minute
         FROM maintenance_schedules
         WHERE organization_id = $1 AND id = $2
         FOR UPDATE",
    )
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((was_enabled, revision, window_start, window_end)) = previous else {
        return Err(PersistenceError::NotFound {
            entity: "maintenance schedule",
            id: id.to_string(),
        });
    };
    if revision != update.expected_revision {
        return Err(PersistenceError::ConfigRevisionConflict {
            entity: "maintenance schedule",
            id: id.to_string(),
            expected: update.expected_revision,
            actual: revision,
        });
    }
    // Sample after the lock: a concurrent admission may already have recorded
    // the next occurrence and advanced the cursor. Using an instant from before
    // the wait could rewind the cursor onto that recorded occurrence, which
    // would then fail the occurrence uniqueness constraint on every pass.
    let now = clock.now(&mut tx).await?;
    let old_window = window_from_row(window_start, window_end)?;
    let timing = update.timing.as_ref();
    let window = update.window.flatten();
    let row = sqlx::query_as::<_, ScheduleRow>(concat!(
        "UPDATE maintenance_schedules
         SET cron_expression = COALESCE($3, cron_expression),
             time_zone = COALESCE($4, time_zone),
             window_start_minute = CASE WHEN $5
                 THEN $6 ELSE window_start_minute END,
             window_end_minute = CASE WHEN $5
                 THEN $7 ELSE window_end_minute END,
             enabled = COALESCE($8, enabled),
             revision = revision + 1,
             updated_at = clock_timestamp()
         WHERE organization_id = $1 AND id = $2 AND revision = $9
         RETURNING ",
        schedule_columns!()
    ))
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .bind(timing.map(|timing| timing.expression.as_str()))
    .bind(timing.map(|timing| timing.time_zone.as_str()))
    .bind(update.window.is_some())
    .bind(window.map(|window| i32::from(window.start_minute())))
    .bind(window.map(|window| i32::from(window.end_minute())))
    .bind(update.enabled)
    .bind(update.expected_revision)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("maintenance schedule", error))?;
    let schedule: MaintenanceSchedule = row.try_into()?;
    let window_changed = update.window.is_some() && schedule.window != old_window;
    if !schedule.enabled {
        clear_state(&mut tx, &schedule).await?;
    } else if update.timing.is_some() || (!was_enabled && update.enabled == Some(true)) {
        arm_state(&mut tx, &schedule, now).await?;
    } else if window_changed {
        rearm_future_cursor(&mut tx, &schedule, now).await?;
    }
    tx.commit().await?;
    Ok(schedule)
}

/// Recomputes a future cursor after a window change.
///
/// An outside-window skip may have advanced `next_due_at` to the next opening,
/// so clearing or widening the window must bring the cursor back to the next
/// cron occurrence. Only a cursor **after** `now` is moved: an overdue cursor
/// must still be evaluated under the new window so a due run is not dropped.
async fn rearm_future_cursor(
    conn: &mut PgConnection,
    schedule: &MaintenanceSchedule,
    now: DateTime<Utc>,
) -> Result<(), PersistenceError> {
    let due: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
        "SELECT next_due_at FROM maintenance_schedule_state
         WHERE organization_id = $1 AND schedule_id = $2
         FOR UPDATE",
    )
    .bind(schedule.organization_id.as_uuid())
    .bind(schedule.id.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    let Some(Some(due)) = due else {
        return Ok(());
    };
    if due <= now {
        return Ok(());
    }
    let Some(next) = next_cursor(conn, schedule, now).await? else {
        return Ok(());
    };
    if next < due {
        sqlx::query(
            "UPDATE maintenance_schedule_state
             SET next_due_at = $3, updated_at = clock_timestamp()
             WHERE organization_id = $1 AND schedule_id = $2",
        )
        .bind(schedule.organization_id.as_uuid())
        .bind(schedule.id.as_uuid())
        .bind(next)
        .execute(conn)
        .await?;
    }
    Ok(())
}

/// The latest instant already represented by an occurrence for this schedule.
///
/// A coalesced evaluation represents every missed instant through
/// `coalesced_until`, so that column wins when present. An empty history means
/// nothing has been evaluated yet.
async fn latest_evaluated(
    conn: &mut PgConnection,
    schedule: &MaintenanceSchedule,
) -> Result<Option<DateTime<Utc>>, PersistenceError> {
    Ok(sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
        "SELECT max(COALESCE(coalesced_until, scheduled_for))
         FROM maintenance_schedule_occurrences
         WHERE organization_id = $1 AND schedule_id = $2",
    )
    .bind(schedule.organization_id.as_uuid())
    .bind(schedule.id.as_uuid())
    .fetch_one(&mut *conn)
    .await?)
}

/// The first cron occurrence strictly after both `now` and the latest evaluated
/// occurrence.
///
/// The occurrence bound makes cursor decisions independent of which clock
/// produced `now`. Admission uses the coordinator clock while schedule updates
/// use the database clock; if the update clock is behind an occurrence that
/// admission already recorded, computing from `now` alone would select that
/// recorded instant again and every later admission would fail its uniqueness
/// constraint. Bounding by the persisted history prevents that rewind without
/// requiring the two clocks to agree.
async fn next_cursor(
    conn: &mut PgConnection,
    schedule: &MaintenanceSchedule,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, PersistenceError> {
    let latest = latest_evaluated(conn, schedule).await?;
    let after = latest.map_or(now, |latest| latest.max(now));
    Ok(schedule.timing.next_occurrence(after))
}

/// Reads the database clock, so arming and advancing share one time source.
async fn database_now(conn: &mut PgConnection) -> Result<DateTime<Utc>, PersistenceError> {
    Ok(sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(conn)
        .await?)
}

/// Sets the schedule's next due instant to its first occurrence after `now` and
/// after the latest evaluated occurrence.
async fn arm_state(
    conn: &mut PgConnection,
    schedule: &MaintenanceSchedule,
    now: DateTime<Utc>,
) -> Result<(), PersistenceError> {
    let next = next_cursor(conn, schedule, now).await?;
    sqlx::query(
        "INSERT INTO maintenance_schedule_state (organization_id, schedule_id, next_due_at)
         VALUES ($1, $2, $3)
         ON CONFLICT (organization_id, schedule_id)
         DO UPDATE SET next_due_at = EXCLUDED.next_due_at,
                       updated_at = clock_timestamp()",
    )
    .bind(schedule.organization_id.as_uuid())
    .bind(schedule.id.as_uuid())
    .bind(next)
    .execute(conn)
    .await?;
    Ok(())
}

async fn clear_state(
    conn: &mut PgConnection,
    schedule: &MaintenanceSchedule,
) -> Result<(), PersistenceError> {
    sqlx::query(
        "DELETE FROM maintenance_schedule_state
         WHERE organization_id = $1 AND schedule_id = $2",
    )
    .bind(schedule.organization_id.as_uuid())
    .bind(schedule.id.as_uuid())
    .execute(conn)
    .await?;
    Ok(())
}

/// Requires `principal` to be a service account that may run maintenance for
/// `table_id` right now.
pub(crate) async fn require_execution_identity(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal: PrincipalId,
    table_id: TableId,
) -> Result<(), PersistenceError> {
    let service = tenancy::principal(conn, organization_id, principal).await?;
    let eligible = service.kind == PrincipalKind::Service
        && grants::has_permission(
            conn,
            organization_id,
            principal,
            Permission::SubmitJobs,
            ResourceScope::Table(table_id),
        )
        .await?;
    if eligible {
        Ok(())
    } else {
        Err(PersistenceError::IneligibleServiceIdentity {
            principal,
            table: table_id,
        })
    }
}

/// Distinguishes "the row is gone" from "the row moved to another revision".
async fn revision_or_missing(
    conn: &mut PgConnection,
    query: &'static str,
    organization_id: OrganizationId,
    id: Uuid,
    expected: i64,
    entity: &'static str,
    id_text: String,
) -> Result<PersistenceError, PersistenceError> {
    let actual: Option<i64> = sqlx::query_scalar(query)
        .bind(organization_id.as_uuid())
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
    match actual {
        Some(actual) => Ok(PersistenceError::ConfigRevisionConflict {
            entity,
            id: id_text,
            expected,
            actual,
        }),
        None => Ok(PersistenceError::NotFound {
            entity,
            id: id_text,
        }),
    }
}
