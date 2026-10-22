//! Maintenance policies and schedules (BS-019.T1).
//!
//! Configuration, not execution: these routes create, read, and edit the
//! policy and schedule definitions that BS-019.T2 will turn into occurrences.
//! Every mutation is audited in the same transaction as the change, and edits
//! name the revision the caller read, so a stale editor fails instead of
//! overwriting a newer decision.
//!
//! Cron expressions and time zones are validated here, before persistence, and
//! reported with a typed reason so the UI can point at the offending field.
//! Schedule creation also requires a service principal that is a service
//! account with a current `jobs:submit` grant covering the table.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use bergsmith_common::audit::{ActorKind, AuditAction, AuditOutcome};
use bergsmith_common::ids::{PolicyId, ScheduleId, TableId};
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_common::policy::{MaintenanceOperation, PolicyError, PolicySpec};
use bergsmith_common::schedule::{CronSchedule, ScheduleError};
use bergsmith_common::window::MaintenanceWindow;
use bergsmith_persistence::audit::NewAuditEvent;
use bergsmith_persistence::schedules::{
    self, MaintenancePolicy, MaintenanceSchedule, NewMaintenancePolicy, NewMaintenanceSchedule,
    PolicyUpdate, ScheduleUpdate,
};
use chrono::SecondsFormat;
use serde::Deserialize;
use serde_json::{Value, json};

use super::pagination::{Page, PageQuery};
use super::{ApiError, ApiState, Caller, json_body, resources};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreatePolicyRequest {
    name: String,
    operation: Option<String>,
    target_file_size_bytes: Option<u64>,
    min_input_files: Option<u32>,
    max_input_bytes: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdatePolicyRequest {
    revision: i64,
    name: Option<String>,
    operation: Option<String>,
    target_file_size_bytes: Option<u64>,
    min_input_files: Option<u32>,
    max_input_bytes: Option<u64>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateScheduleRequest {
    policy_id: PolicyId,
    table_id: TableId,
    cron_expression: String,
    time_zone: String,
    window: Option<WindowBody>,
    service_principal_id: bergsmith_common::ids::PrincipalId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdateScheduleRequest {
    revision: i64,
    cron_expression: Option<String>,
    time_zone: Option<String>,
    /// Absent leaves the stored window alone, `null` clears it, and an object
    /// replaces it.
    #[serde(default, deserialize_with = "double_option")]
    #[expect(
        clippy::option_option,
        reason = "absent, null, and an object are three distinct PATCH states"
    )]
    window: Option<Option<WindowBody>>,
    enabled: Option<bool>,
}

/// A daily local-time range in the schedule's time zone.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WindowBody {
    /// Inclusive minute after local midnight, `0..=1439`.
    ///
    /// Parsed as a wider integer so every out-of-day value reaches the window
    /// validator and reports `invalid_maintenance_window`, rather than failing
    /// the JSON body with the generic `invalid_request`.
    start_minute: i64,
    /// Exclusive minute after local midnight, `0..=1439`. A value smaller than
    /// the start wraps past midnight.
    end_minute: i64,
}

impl WindowBody {
    /// Validates the pair into a window.
    fn into_window(self) -> Result<MaintenanceWindow, ApiError> {
        let start =
            u16::try_from(self.start_minute).map_err(|_| ApiError::invalid_maintenance_window())?;
        let end =
            u16::try_from(self.end_minute).map_err(|_| ApiError::invalid_maintenance_window())?;
        MaintenanceWindow::new(start, end).map_err(|_| ApiError::invalid_maintenance_window())
    }
}

/// Distinguishes an absent field from an explicit `null` in a PATCH body.
#[expect(
    clippy::option_option,
    reason = "this is the standard serde double-option deserializer"
)]
pub(super) fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Lists the organization's policies in identifier order.
pub(super) async fn policies(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<PageQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let page = Page::parse(
        query.map_err(|_| ApiError::invalid())?.0,
        caller.organization_id,
        caller.principal_id,
        "policies",
    )?;
    let after = page
        .after
        .as_deref()
        .map(|raw| raw.parse::<PolicyId>().map_err(|_| ApiError::invalid()))
        .transpose()?;
    authorize(&state, &caller, ResourceScope::Organization).await?;
    let mut conn = state.database.pool().acquire().await?;
    let limit = i64::try_from(page.limit.saturating_add(1)).map_err(|_| ApiError::invalid())?;
    let rows = schedules::policies(&mut conn, caller.organization_id, after, limit).await?;
    Ok(Json(
        page.finish(
            rows.iter()
                .map(|policy| (policy.id.to_string(), policy_json(policy)))
                .collect(),
        )?,
    ))
}

/// Reads one policy.
pub(super) async fn policy(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<uuid::Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = PolicyId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    authorize(&state, &caller, ResourceScope::Organization).await?;
    let mut conn = state.database.pool().acquire().await?;
    let stored = schedules::policy(&mut conn, caller.organization_id, id).await?;
    Ok(Json(policy_json(&stored)))
}

/// Creates a policy and audits it in the same transaction.
pub(super) async fn create_policy(
    State(state): State<ApiState>,
    caller: Caller,
    body: Result<Json<CreatePolicyRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let request = json_body(body)?;
    authorize(&state, &caller, ResourceScope::Organization).await?;
    let specification = specification(
        request.operation.as_deref(),
        request.target_file_size_bytes,
        request.min_input_files,
        request.max_input_bytes,
    )?;
    let id = PolicyId::generate();
    let mut tx = state.database.pool().begin().await?;
    let created = schedules::create_policy(
        &mut tx,
        &NewMaintenancePolicy {
            id,
            organization_id: caller.organization_id,
            name: request.name,
            specification,
            created_by: caller.principal_id,
        },
    )
    .await?;
    record(
        &mut tx,
        &caller,
        AuditAction::PolicyCreated,
        "maintenance_policy",
        &id.to_string(),
        json!({
            "name": created.name,
            "operation": created.specification.operation.as_str(),
            "target_file_size_bytes": created.specification.target_file_size_bytes,
            "min_input_files": created.specification.min_input_files,
            "max_input_bytes": created.specification.max_input_bytes,
            "revision": created.revision,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(policy_json(&created))))
}

/// Edits a policy when the revision still matches.
pub(super) async fn update_policy(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<uuid::Uuid>, axum::extract::rejection::PathRejection>,
    body: Result<Json<UpdatePolicyRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = PolicyId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    let request = json_body(body)?;
    if request.revision < 1 {
        return Err(ApiError::invalid());
    }
    let changes_specification = request.operation.is_some()
        || request.target_file_size_bytes.is_some()
        || request.min_input_files.is_some()
        || request.max_input_bytes.is_some();
    if request.name.is_none() && !changes_specification && request.enabled.is_none() {
        return Err(ApiError::invalid());
    }
    authorize(&state, &caller, ResourceScope::Organization).await?;
    let mut tx = state.database.pool().begin().await?;
    let current = schedules::policy(&mut tx, caller.organization_id, id).await?;
    let specification = if changes_specification {
        let operation = match request.operation.as_deref() {
            Some(name) => parse_operation(Some(name))?,
            None => current.specification.operation,
        };
        Some(
            PolicySpec::new(
                operation,
                request
                    .target_file_size_bytes
                    .unwrap_or(current.specification.target_file_size_bytes),
                request
                    .min_input_files
                    .unwrap_or(current.specification.min_input_files),
                request
                    .max_input_bytes
                    .unwrap_or(current.specification.max_input_bytes),
            )
            .map_err(|error| policy_error(&error))?,
        )
    } else {
        None
    };
    let updated = schedules::update_policy(
        &mut tx,
        caller.organization_id,
        id,
        &PolicyUpdate {
            name: request.name,
            specification,
            enabled: request.enabled,
            expected_revision: request.revision,
        },
    )
    .await?;
    record(
        &mut tx,
        &caller,
        AuditAction::PolicyUpdated,
        "maintenance_policy",
        &id.to_string(),
        json!({
            "name": updated.name,
            "operation": updated.specification.operation.as_str(),
            "target_file_size_bytes": updated.specification.target_file_size_bytes,
            "min_input_files": updated.specification.min_input_files,
            "max_input_bytes": updated.specification.max_input_bytes,
            "revision": updated.revision,
            "enabled": updated.enabled,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(policy_json(&updated)))
}

/// Lists the organization's schedules in identifier order.
pub(super) async fn schedules(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<PageQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let page = Page::parse(
        query.map_err(|_| ApiError::invalid())?.0,
        caller.organization_id,
        caller.principal_id,
        "schedules",
    )?;
    let after = page
        .after
        .as_deref()
        .map(|raw| raw.parse::<ScheduleId>().map_err(|_| ApiError::invalid()))
        .transpose()?;
    authorize(&state, &caller, ResourceScope::Organization).await?;
    let mut conn = state.database.pool().acquire().await?;
    let limit = i64::try_from(page.limit.saturating_add(1)).map_err(|_| ApiError::invalid())?;
    let rows = schedules::schedules(&mut conn, caller.organization_id, after, limit).await?;
    Ok(Json(
        page.finish(
            rows.iter()
                .map(|schedule| (schedule.id.to_string(), schedule_json(schedule)))
                .collect(),
        )?,
    ))
}

/// Reads one schedule. Requires organization-wide `schedules:manage`.
pub(super) async fn schedule(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<uuid::Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = ScheduleId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    authorize(&state, &caller, ResourceScope::Organization).await?;
    let mut conn = state.database.pool().acquire().await?;
    let stored = schedules::schedule(&mut conn, caller.organization_id, id).await?;
    Ok(Json(schedule_json(&stored)))
}

/// Creates a schedule and audits it in the same transaction.
pub(super) async fn create_schedule(
    State(state): State<ApiState>,
    caller: Caller,
    body: Result<Json<CreateScheduleRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let request = json_body(body)?;
    // Scheduling is organization configuration: an administrator needs an
    // organization-wide grant, not one scoped to a single table.
    authorize(&state, &caller, ResourceScope::Organization).await?;
    let timing = timing(&request.cron_expression, &request.time_zone)?;
    let window = request.window.map(WindowBody::into_window).transpose()?;
    let id = ScheduleId::generate();
    let mut tx = state.database.pool().begin().await?;
    schedules::policy(&mut tx, caller.organization_id, request.policy_id).await?;
    let created = schedules::create_schedule(
        &mut tx,
        &NewMaintenanceSchedule {
            id,
            organization_id: caller.organization_id,
            policy_id: request.policy_id,
            table_id: request.table_id,
            timing,
            window,
            service_principal_id: request.service_principal_id,
            created_by: caller.principal_id,
        },
    )
    .await?;
    record(
        &mut tx,
        &caller,
        AuditAction::ScheduleCreated,
        "maintenance_schedule",
        &id.to_string(),
        json!({
            "policy_id": created.policy_id,
            "table_id": created.table_id,
            "cron_expression": created.timing.expression.as_str(),
            "time_zone": created.timing.time_zone.as_str(),
            "window": window_json(created.window),
            "revision": created.revision,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(schedule_json(&created))))
}

/// Edits a schedule when the revision still matches.
pub(super) async fn update_schedule(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<uuid::Uuid>, axum::extract::rejection::PathRejection>,
    body: Result<Json<UpdateScheduleRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = ScheduleId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    let request = json_body(body)?;
    if request.revision < 1 {
        return Err(ApiError::invalid());
    }
    if request.cron_expression.is_none()
        && request.time_zone.is_none()
        && request.window.is_none()
        && request.enabled.is_none()
    {
        return Err(ApiError::invalid());
    }
    authorize(&state, &caller, ResourceScope::Organization).await?;
    let mut tx = state.database.pool().begin().await?;
    let current = schedules::schedule(&mut tx, caller.organization_id, id).await?;
    let timing = match (request.cron_expression, request.time_zone) {
        (None, None) => None,
        (expression, time_zone) => Some(timing(
            expression
                .as_deref()
                .unwrap_or(current.timing.expression.as_str()),
            time_zone
                .as_deref()
                .unwrap_or(current.timing.time_zone.as_str()),
        )?),
    };
    let window = request
        .window
        .map(|window| window.map(WindowBody::into_window).transpose())
        .transpose()?;
    let updated = schedules::update_schedule(
        &mut tx,
        caller.organization_id,
        id,
        &ScheduleUpdate {
            timing,
            window,
            enabled: request.enabled,
            expected_revision: request.revision,
        },
    )
    .await?;
    record(
        &mut tx,
        &caller,
        AuditAction::ScheduleUpdated,
        "maintenance_schedule",
        &id.to_string(),
        json!({
            "cron_expression": updated.timing.expression.as_str(),
            "time_zone": updated.timing.time_zone.as_str(),
            "window": window_json(updated.window),
            "revision": updated.revision,
            "enabled": updated.enabled,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(schedule_json(&updated)))
}

/// Requires `schedules:manage` over `resource`.
///
/// The check uses its own pooled connection, so a denial is audited in
/// autocommit and survives the caller not opening a transaction.
async fn authorize(
    state: &ApiState,
    caller: &Caller,
    resource: ResourceScope,
) -> Result<(), ApiError> {
    let mut conn = state.database.pool().acquire().await?;
    resources::require(&mut conn, &caller.0, Permission::ManageSchedules, resource).await
}

/// Parses a policy's operation, defaulting to compaction.
fn parse_operation(value: Option<&str>) -> Result<MaintenanceOperation, ApiError> {
    match value {
        None => Ok(MaintenanceOperation::Compact),
        Some(value) => MaintenanceOperation::from_name(value).ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_policy_operation",
                "choose_supported_operation",
            )
        }),
    }
}

/// Builds a validated policy specification from optional request fields.
fn specification(
    operation_name: Option<&str>,
    target_file_size_bytes: Option<u64>,
    min_input_files: Option<u32>,
    max_input_bytes: Option<u64>,
) -> Result<PolicySpec, ApiError> {
    let defaults = PolicySpec::default();
    let operation = parse_operation(operation_name)?;
    PolicySpec::new(
        operation,
        target_file_size_bytes.unwrap_or(defaults.target_file_size_bytes),
        min_input_files.unwrap_or(defaults.min_input_files),
        max_input_bytes.unwrap_or(defaults.max_input_bytes),
    )
    .map_err(|error| policy_error(&error))
}

/// Reports a rejected policy parameter with the field the caller must change.
fn policy_error(error: &PolicyError) -> ApiError {
    match error {
        PolicyError::MaxInputBelowTarget { .. } => ApiError::new(
            StatusCode::BAD_REQUEST,
            "run_budget_below_target_file_size",
            "increase_run_budget",
        ),
        PolicyError::TargetFileSize { .. }
        | PolicyError::MinInputFiles { .. }
        | PolicyError::MaxInputBytes { .. } => ApiError::invalid_policy_parameters(),
    }
}

/// Parses a timing contract, reporting which part was rejected.
fn timing(expression: &str, time_zone: &str) -> Result<CronSchedule, ApiError> {
    CronSchedule::parse(expression, time_zone).map_err(|error| match error {
        ScheduleError::UnknownTimeZone { .. } => ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_time_zone",
            "use_iana_time_zone",
        ),
        _ => ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_cron_expression",
            "correct_cron_expression",
        ),
    })
}

/// The public representation of a policy.
fn policy_json(policy: &MaintenancePolicy) -> Value {
    json!({
        "id": policy.id,
        "name": policy.name,
        "operation": policy.specification.operation.as_str(),
        "target_file_size_bytes": policy.specification.target_file_size_bytes,
        "min_input_files": policy.specification.min_input_files,
        "max_input_bytes": policy.specification.max_input_bytes,
        "enabled": policy.enabled,
        "revision": policy.revision,
        "created_at": policy.created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        "updated_at": policy.updated_at.to_rfc3339_opts(SecondsFormat::Millis, true),
    })
}

/// The public representation of a schedule.
fn schedule_json(schedule: &MaintenanceSchedule) -> Value {
    json!({
        "id": schedule.id,
        "policy_id": schedule.policy_id,
        "table_id": schedule.table_id,
        "cron_expression": schedule.timing.expression.as_str(),
        "time_zone": schedule.timing.time_zone.as_str(),
        "window": window_json(schedule.window),
        "service_principal_id": schedule.service_principal_id,
        "enabled": schedule.enabled,
        "revision": schedule.revision,
        "created_at": schedule.created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        "updated_at": schedule.updated_at.to_rfc3339_opts(SecondsFormat::Millis, true),
    })
}

/// The public representation of a schedule's window, or `null` for any time.
fn window_json(window: Option<MaintenanceWindow>) -> Value {
    match window {
        Some(window) => json!({
            "start_minute": window.start_minute(),
            "end_minute": window.end_minute(),
        }),
        None => Value::Null,
    }
}

/// Appends one configuration audit record for this caller.
async fn record(
    conn: &mut sqlx::PgConnection,
    caller: &Caller,
    action: AuditAction,
    resource_type: &'static str,
    resource_id: &str,
    detail: Value,
) -> Result<(), ApiError> {
    bergsmith_persistence::audit::record(
        conn,
        &NewAuditEvent {
            organization_id: Some(caller.0.organization_id),
            actor_principal_id: Some(caller.0.principal_id),
            actor_kind: ActorKind::User,
            action: action.as_str(),
            resource_type,
            resource_id: Some(resource_id),
            outcome: AuditOutcome::Allowed,
            detail,
        },
    )
    .await?;
    Ok(())
}
