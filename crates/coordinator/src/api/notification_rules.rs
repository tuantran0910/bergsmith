//! Notification rules (BS-029.T2, ADR 0060).
//!
//! | Route | Purpose |
//! |---|---|
//! | `GET /api/v1/notifications/rules` | Page through rules |
//! | `POST /api/v1/notifications/rules` | Register one |
//! | `GET /api/v1/notifications/rules/{id}` | One rule |
//! | `PATCH /api/v1/notifications/rules/{id}` | Edit it when the revision matches |
//!
//! Every route requires organization-wide `notifications:manage`. A rule binds
//! one event kind to one destination and at most one filter: a connection, a
//! worker-pool name, or a table. The event kind cannot change after creation;
//! a repeat threshold applies only to the kinds that recur.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bergsmith_common::audit::{AuditAction, AuditOutcome};
use bergsmith_common::ids::{
    CatalogConnectionId, NotificationDestinationId, NotificationRuleId, TableId,
};
use bergsmith_common::notifications::EventKind;
use bergsmith_persistence::notifications::{
    self, NewNotificationRule, NotificationRule, NotificationRuleUpdate, RuleFilter,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::notifications::{record, require_notifications, validate_name};
use super::pagination::{Page, PageQuery};
use super::{ApiError, ApiState, Caller, json_body};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListRules {
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateRule {
    name: String,
    destination_id: Uuid,
    event_kind: String,
    filter: Option<RuleFilterBody>,
    repeat_threshold: Option<i32>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdateRule {
    revision: i64,
    name: Option<String>,
    destination_id: Option<Uuid>,
    /// Absent leaves the stored filter alone, `null` clears it, and an object
    /// replaces it.
    #[serde(default, deserialize_with = "super::scheduling::double_option")]
    #[expect(
        clippy::option_option,
        reason = "absent, null, and an object are three distinct PATCH states"
    )]
    filter: Option<Option<RuleFilterBody>>,
    /// Absent leaves the stored threshold alone, `null` clears it, and a
    /// number replaces it.
    #[serde(default, deserialize_with = "super::scheduling::double_option")]
    #[expect(
        clippy::option_option,
        reason = "absent and null are distinct PATCH states"
    )]
    repeat_threshold: Option<Option<i32>>,
    enabled: Option<bool>,
}

/// The one optional narrowing filter on a rule.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RuleFilterBody {
    connection_id: Option<Uuid>,
    pool: Option<String>,
    table_id: Option<Uuid>,
}

fn document(rule: &NotificationRule) -> Value {
    json!({
        "id": rule.id,
        "name": rule.name,
        "destination_id": rule.destination_id,
        "event_kind": rule.event_kind.as_str(),
        "filter": filter_json(rule),
        "repeat_threshold": rule.repeat_threshold,
        "enabled": rule.enabled,
        "revision": rule.revision,
        "created_by": rule.created_by,
        "created_at": rule.created_at,
        "updated_at": rule.updated_at,
    })
}

fn filter_json(rule: &NotificationRule) -> Value {
    if let Some(connection_id) = rule.catalog_connection_id {
        json!({"connection_id": connection_id})
    } else if let Some(pool) = &rule.worker_pool {
        json!({"pool": pool})
    } else if let Some(table_id) = rule.table_identity_id {
        json!({"table_id": table_id})
    } else {
        Value::Null
    }
}

fn rule_id(
    path: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<NotificationRuleId, ApiError> {
    Ok(NotificationRuleId::from_uuid(
        path.map_err(|_| ApiError::invalid())?.0,
    ))
}

/// Parses a stable event kind.
fn event_kind(raw: &str) -> Result<EventKind, ApiError> {
    EventKind::from_name(raw.trim()).ok_or_else(ApiError::invalid)
}

/// Checks the repeat threshold against the event kind.
fn validate_threshold(kind: EventKind, threshold: Option<i32>) -> Result<(), ApiError> {
    match threshold {
        Some(value) if value >= 1 && kind.allows_repeat_threshold() => Ok(()),
        Some(_) => Err(ApiError::invalid()),
        None => Ok(()),
    }
}

/// A worker-pool name, matching the ledger's own bound.
fn validate_pool(pool: &str) -> Result<String, ApiError> {
    let valid_start = pool
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit());
    if !valid_start
        || pool.len() > 63
        || !pool.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
    {
        return Err(ApiError::invalid());
    }
    Ok(pool.to_owned())
}

/// Requires exactly one filter field.
fn rule_filter(body: RuleFilterBody) -> Result<RuleFilter, ApiError> {
    let mut set = 0;
    let mut filter = None;
    if let Some(id) = body.connection_id {
        set += 1;
        filter = Some(RuleFilter::Connection(CatalogConnectionId::from_uuid(id)));
    }
    if let Some(pool) = body.pool {
        set += 1;
        filter = Some(RuleFilter::Pool(validate_pool(pool.trim())?));
    }
    if let Some(id) = body.table_id {
        set += 1;
        filter = Some(RuleFilter::Table(TableId::from_uuid(id)));
    }
    if set != 1 {
        return Err(ApiError::invalid());
    }
    filter.ok_or_else(ApiError::invalid)
}

fn destination_id(raw: Uuid) -> NotificationDestinationId {
    NotificationDestinationId::from_uuid(raw)
}

/// `GET /api/v1/notifications/rules`.
pub(super) async fn list(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<ListRules>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let page = Page::parse(
        PageQuery::new(query.limit, query.cursor),
        caller.organization_id,
        caller.principal_id,
        "notification-rules",
    )?;
    let after = page
        .after
        .as_deref()
        .map(|after| {
            Uuid::parse_str(after)
                .map(NotificationRuleId::from_uuid)
                .map_err(|_| ApiError::invalid())
        })
        .transpose()?;
    let mut conn = state.database.pool().acquire().await?;
    require_notifications(&mut conn, &caller).await?;
    let rows = notifications::rules(
        &mut conn,
        caller.organization_id,
        after,
        i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?,
    )
    .await?;
    Ok(Json(
        page.finish(
            rows.iter()
                .map(|rule| (rule.id.to_string(), document(rule)))
                .collect(),
        )?,
    ))
}

/// `POST /api/v1/notifications/rules`.
pub(super) async fn create(
    State(state): State<ApiState>,
    caller: Caller,
    body: Result<Json<CreateRule>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let body = json_body(body)?;
    let mut conn = state.database.pool().acquire().await?;
    require_notifications(&mut conn, &caller).await?;
    drop(conn);
    let name = validate_name(&body.name)?;
    let kind = event_kind(&body.event_kind)?;
    validate_threshold(kind, body.repeat_threshold)?;
    let filter = body.filter.map(rule_filter).transpose()?;
    let destination = destination_id(body.destination_id);
    let id = NotificationRuleId::generate();
    let mut tx = state.database.pool().begin().await?;
    let created = notifications::create_rule(
        &mut tx,
        &NewNotificationRule {
            id,
            organization_id: caller.organization_id,
            name,
            destination_id: destination,
            event_kind: kind,
            filter,
            repeat_threshold: body.repeat_threshold,
            enabled: body.enabled.unwrap_or(true),
            created_by: caller.principal_id,
        },
    )
    .await?;
    record(
        &mut tx,
        &caller,
        AuditAction::NotificationRuleCreated,
        "notification_rule",
        &id.to_string(),
        AuditOutcome::Allowed,
        json!({
            "name": created.name,
            "destination_id": created.destination_id,
            "event_kind": created.event_kind.as_str(),
            "filter": filter_json(&created),
            "repeat_threshold": created.repeat_threshold,
            "enabled": created.enabled,
            "revision": created.revision,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        [(
            header::LOCATION,
            format!("/api/v1/notifications/rules/{id}"),
        )],
        Json(document(&created)),
    )
        .into_response())
}

/// `GET /api/v1/notifications/rules/{id}`.
pub(super) async fn get(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = rule_id(id)?;
    let mut conn = state.database.pool().acquire().await?;
    require_notifications(&mut conn, &caller).await?;
    let rule = notifications::rule(&mut conn, caller.organization_id, id).await?;
    Ok(Json(document(&rule)))
}

/// `PATCH /api/v1/notifications/rules/{id}`.
pub(super) async fn update(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
    body: Result<Json<UpdateRule>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = rule_id(id)?;
    let body = json_body(body)?;
    if body.revision < 1 {
        return Err(ApiError::invalid());
    }
    if body.name.is_none()
        && body.destination_id.is_none()
        && body.filter.is_none()
        && body.repeat_threshold.is_none()
        && body.enabled.is_none()
    {
        return Err(ApiError::invalid());
    }
    let mut conn = state.database.pool().acquire().await?;
    require_notifications(&mut conn, &caller).await?;
    let current = notifications::rule(&mut conn, caller.organization_id, id).await?;
    // Release the authorization connection before taking the transaction.
    drop(conn);
    let name = body.name.as_deref().map(validate_name).transpose()?;
    let destination = body.destination_id.map(destination_id);
    let filter = match body.filter {
        None => None,
        Some(None) => Some(None),
        Some(Some(filter)) => Some(Some(rule_filter(filter)?)),
    };
    if let Some(Some(threshold)) = body.repeat_threshold {
        validate_threshold(current.event_kind, Some(threshold))?;
    }
    let mut tx = state.database.pool().begin().await?;
    let updated = notifications::update_rule(
        &mut tx,
        caller.organization_id,
        id,
        &NotificationRuleUpdate {
            name,
            destination_id: destination,
            filter,
            repeat_threshold: body.repeat_threshold,
            enabled: body.enabled,
            expected_revision: body.revision,
        },
    )
    .await?;
    record(
        &mut tx,
        &caller,
        AuditAction::NotificationRuleUpdated,
        "notification_rule",
        &id.to_string(),
        AuditOutcome::Allowed,
        json!({
            "name": updated.name,
            "destination_id": updated.destination_id,
            "event_kind": updated.event_kind.as_str(),
            "filter": filter_json(&updated),
            "repeat_threshold": updated.repeat_threshold,
            "enabled": updated.enabled,
            "revision": updated.revision,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(document(&updated)))
}
