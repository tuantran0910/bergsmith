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
//! timing contract cannot be persisted. Schedule creation additionally refuses
//! a service principal that is not a service account or that holds no grant
//! covering `jobs:submit` for the table.

use bergsmith_common::ids::{OrganizationId, PolicyId, PrincipalId, ScheduleId, TableId};
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_common::policy::{MaintenanceOperation, PolicySpec};
use bergsmith_common::schedule::CronSchedule;
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
        let specification = PolicySpec::new(operation, target_file_size_bytes, min_input_files)
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
        Ok(Self {
            id: ScheduleId::from_uuid(row.id),
            organization_id: OrganizationId::from_uuid(row.organization_id),
            policy_id: PolicyId::from_uuid(row.policy_id),
            table_id: TableId::from_uuid(row.table_identity_id),
            timing,
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
         min_input_files, enabled, revision, created_by, created_at, updated_at"
    };
}

/// Columns selected for [`MaintenanceSchedule`], in [`ScheduleRow`] order.
macro_rules! schedule_columns {
    () => {
        "id, organization_id, policy_id, table_identity_id, cron_expression, \
         time_zone, service_principal_id, enabled, revision, created_by, \
         created_at, updated_at"
    };
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
              min_input_files, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
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
             enabled = COALESCE($7, enabled),
             revision = revision + 1,
             updated_at = clock_timestamp()
         WHERE organization_id = $1 AND id = $2 AND revision = $8
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
              time_zone, service_principal_id, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING ",
        schedule_columns!()
    ))
    .bind(new.id.as_uuid())
    .bind(new.organization_id.as_uuid())
    .bind(new.policy_id.as_uuid())
    .bind(new.table_id.as_uuid())
    .bind(new.timing.expression.as_str())
    .bind(new.timing.time_zone.as_str())
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
    let mut tx = conn.begin().await?;
    // Lock first: the previous enabled value decides whether this update is a
    // real re-enable or a no-op that must preserve an overdue cursor.
    let previous: Option<(bool, i64)> = sqlx::query_as(
        "SELECT enabled, revision FROM maintenance_schedules
         WHERE organization_id = $1 AND id = $2
         FOR UPDATE",
    )
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((was_enabled, revision)) = previous else {
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
    let timing = update.timing.as_ref();
    let row = sqlx::query_as::<_, ScheduleRow>(concat!(
        "UPDATE maintenance_schedules
         SET cron_expression = COALESCE($3, cron_expression),
             time_zone = COALESCE($4, time_zone),
             enabled = COALESCE($5, enabled),
             revision = revision + 1,
             updated_at = clock_timestamp()
         WHERE organization_id = $1 AND id = $2 AND revision = $6
         RETURNING ",
        schedule_columns!()
    ))
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .bind(timing.map(|timing| timing.expression.as_str()))
    .bind(timing.map(|timing| timing.time_zone.as_str()))
    .bind(update.enabled)
    .bind(update.expected_revision)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("maintenance schedule", error))?;
    let schedule: MaintenanceSchedule = row.try_into()?;
    if !schedule.enabled {
        clear_state(&mut tx, &schedule).await?;
    } else if update.timing.is_some() || (!was_enabled && update.enabled == Some(true)) {
        let now = database_now(&mut tx).await?;
        arm_state(&mut tx, &schedule, now).await?;
    }
    tx.commit().await?;
    Ok(schedule)
}

/// Reads the database clock, so arming and advancing share one time source.
async fn database_now(conn: &mut PgConnection) -> Result<DateTime<Utc>, PersistenceError> {
    Ok(sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(conn)
        .await?)
}

/// Sets the schedule's next due instant to its first occurrence after `now`.
async fn arm_state(
    conn: &mut PgConnection,
    schedule: &MaintenanceSchedule,
    now: DateTime<Utc>,
) -> Result<(), PersistenceError> {
    sqlx::query(
        "INSERT INTO maintenance_schedule_state (organization_id, schedule_id, next_due_at)
         VALUES ($1, $2, $3)
         ON CONFLICT (organization_id, schedule_id)
         DO UPDATE SET next_due_at = EXCLUDED.next_due_at,
                       updated_at = clock_timestamp()",
    )
    .bind(schedule.organization_id.as_uuid())
    .bind(schedule.id.as_uuid())
    .bind(schedule.timing.next_occurrence(now))
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
