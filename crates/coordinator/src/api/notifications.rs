//! Notification destinations (BS-029.T1, ADR 0060).
//!
//! | Route | Purpose |
//! |---|---|
//! | `GET /api/v1/notifications/destinations` | Page through destinations |
//! | `POST /api/v1/notifications/destinations` | Register one |
//! | `GET /api/v1/notifications/destinations/{id}` | One destination |
//! | `PATCH /api/v1/notifications/destinations/{id}` | Edit it when the revision matches |
//! | `POST /api/v1/notifications/destinations/{id}/test` | Send one test event |
//!
//! Every route requires organization-wide `notifications:manage`.
//! `url_ref` and `signing_secret_ref` name files under
//! `notifications.secrets_dir`; neither the destination URL (a Slack URL embeds
//! a bearer token in its path) nor the signing key ever appears in a response,
//! an error, or an audit record. A test send is synchronous and bounded by the
//! configured request timeout; it does not enqueue a durable delivery, and its
//! audit row records the outcome whether or not the request was made.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bergsmith_common::audit::{ActorKind, AuditAction, AuditOutcome};
use bergsmith_common::ids::{NotificationDestinationId, NotificationRuleId};
use bergsmith_common::notifications::{
    DestinationKind, DestinationUrl, DestinationUrlError, SecretRef,
};
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::audit::NewAuditEvent;
use bergsmith_persistence::notifications::{
    self, DeliveryFilter, DeliveryState, NewNotificationDestination, NotificationDestination,
    NotificationDestinationUpdate,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::notifications::{
    Delivery, DeliveryError, Event, ResourceRef, SecretError, Target, read_secret,
};

use super::pagination::{Page, PageQuery};
use super::{ApiError, ApiState, Caller, json_body, resources};

/// The longest destination name, in characters.
const MAX_NAME: usize = 200;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListDestinations {
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateDestination {
    name: String,
    kind: String,
    url_ref: String,
    signing_secret_ref: Option<String>,
    enabled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdateDestination {
    revision: i64,
    name: Option<String>,
    url_ref: Option<String>,
    signing_secret_ref: Option<String>,
    enabled: Option<bool>,
}

fn document(destination: &NotificationDestination) -> Value {
    json!({
        "id": destination.id,
        "name": destination.name,
        "kind": destination.kind.as_str(),
        "url_ref": destination.url_ref,
        "signing_secret_ref": destination.signing_secret_ref,
        "enabled": destination.enabled,
        "revision": destination.revision,
        "created_by": destination.created_by,
        "created_at": destination.created_at,
        "updated_at": destination.updated_at,
    })
}

pub(super) async fn require_notifications(
    conn: &mut sqlx::PgConnection,
    caller: &Caller,
) -> Result<(), ApiError> {
    resources::require(
        conn,
        &caller.0,
        Permission::ManageNotifications,
        ResourceScope::Organization,
    )
    .await
}

fn destination_id(
    path: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<NotificationDestinationId, ApiError> {
    Ok(NotificationDestinationId::from_uuid(
        path.map_err(|_| ApiError::invalid())?.0,
    ))
}

pub(super) fn validate_name(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    let mut chars = name.chars();
    let valid_start = chars
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric());
    if !valid_start
        || name.chars().count() > MAX_NAME
        || !name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, ' ' | '.' | '_' | '-')
        })
    {
        return Err(ApiError::invalid());
    }
    Ok(name.to_owned())
}

fn not_allowed() -> ApiError {
    ApiError::permanent(
        StatusCode::BAD_REQUEST,
        "destination_not_allowed",
        "add_the_host_to_notifications_allowed_hosts",
    )
}

fn url_reference_error(error: &SecretError) -> ApiError {
    match error {
        SecretError::NotConfigured => ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "notifications_secrets_dir_unconfigured",
            "configure_notifications_secrets_dir",
        ),
        SecretError::InvalidReference => ApiError::invalid(),
        SecretError::Unavailable { .. } | SecretError::Empty => ApiError::permanent(
            StatusCode::BAD_REQUEST,
            "destination_url_unavailable",
            "write_the_url_file_under_notifications_secrets_dir",
        ),
        SecretError::NotUtf8 | SecretError::TooLarge { .. } => ApiError::permanent(
            StatusCode::BAD_REQUEST,
            "destination_url_invalid",
            "fix_the_url_in_notifications_secrets_dir",
        ),
    }
}

/// Resolves a URL reference and enforces the URL policy on the value.
async fn resolve_url(state: &ApiState, raw: &str) -> Result<(SecretRef, DestinationUrl), ApiError> {
    let reference = SecretRef::parse(raw).map_err(|_| ApiError::invalid())?;
    let text = read_secret(state.notifications.secrets_dir(), reference.as_str())
        .await
        .map_err(|error| url_reference_error(&error))?;
    let url = DestinationUrl::parse(text.expose()).map_err(|_| {
        ApiError::permanent(
            StatusCode::BAD_REQUEST,
            "destination_url_invalid",
            "fix_the_url_in_notifications_secrets_dir",
        )
    })?;
    url.allow(state.notifications.allow_list())
        .map_err(|_| not_allowed())?;
    Ok((reference, url))
}

/// Validates a URL reference for storage. The URL is never returned: only the
/// reference reaches the ledger, an audit record, or a response.
async fn validate_url_ref(state: &ApiState, raw: &str) -> Result<String, ApiError> {
    let (reference, _) = resolve_url(state, raw).await?;
    Ok(reference.as_str().to_owned())
}

/// Resolves a URL reference to the URL a send uses.
async fn resolved_url(state: &ApiState, raw: &str) -> Result<String, ApiError> {
    let (_, url) = resolve_url(state, raw).await?;
    Ok(url.as_str().to_owned())
}

fn validate_secret_ref(
    kind: DestinationKind,
    raw: Option<&str>,
) -> Result<Option<String>, ApiError> {
    match (kind.requires_signing_secret(), raw) {
        (true, None) | (false, Some(_)) => Err(ApiError::invalid()),
        (false, None) => Ok(None),
        (true, Some(reference)) => SecretRef::parse(reference)
            .map(|reference| Some(reference.as_str().to_owned()))
            .map_err(|_| ApiError::invalid()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListDeliveries {
    limit: Option<usize>,
    cursor: Option<String>,
    state: Option<String>,
    rule_id: Option<Uuid>,
    destination_id: Option<Uuid>,
}

fn delivery_state(raw: &str) -> Result<DeliveryState, ApiError> {
    match raw {
        "pending" => Ok(DeliveryState::Pending),
        "delivered" => Ok(DeliveryState::Delivered),
        "exhausted" => Ok(DeliveryState::Exhausted),
        _ => Err(ApiError::invalid()),
    }
}

fn delivery_document(delivery: &notifications::NotificationDelivery) -> Value {
    json!({
        "id": delivery.id,
        "rule_id": delivery.rule_id,
        "destination_id": delivery.destination_id,
        "destination_name": delivery.destination_name,
        "destination_kind": delivery.destination_kind.as_str(),
        "event_kind": delivery.event_kind.as_str(),
        "occurred_at": delivery.occurred_at,
        "summary": delivery.summary,
        "resource": {
            "kind": delivery.resource_kind,
            "id": delivery.resource_id,
            "label": delivery.resource_label,
        },
        "reason": delivery.reason,
        "state": delivery.state.as_str().to_ascii_lowercase(),
        "attempt_count": delivery.attempt_count,
        "next_attempt_at": delivery.next_attempt_at,
        "last_error": delivery.last_error,
        "created_at": delivery.created_at,
        "updated_at": delivery.updated_at,
    })
}

/// Parses a delivery page cursor: the last row's creation instant and id.
fn delivery_after(raw: &str) -> Result<(DateTime<Utc>, Uuid), ApiError> {
    let (micros, id) = raw.split_once('|').ok_or_else(ApiError::invalid)?;
    let micros: i64 = micros.parse().map_err(|_| ApiError::invalid())?;
    let created_at = DateTime::from_timestamp_micros(micros).ok_or_else(ApiError::invalid)?;
    let id = Uuid::parse_str(id).map_err(|_| ApiError::invalid())?;
    Ok((created_at, id))
}

/// `GET /api/v1/notifications/deliveries`.
pub(super) async fn list_deliveries(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<ListDeliveries>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let page = Page::parse(
        PageQuery::new(query.limit, query.cursor),
        caller.organization_id,
        caller.principal_id,
        "notifications-deliveries",
    )?;
    let after = page.after.as_deref().map(delivery_after).transpose()?;
    let filter = DeliveryFilter {
        state: query.state.as_deref().map(delivery_state).transpose()?,
        rule: query.rule_id.map(NotificationRuleId::from_uuid),
        destination: query
            .destination_id
            .map(NotificationDestinationId::from_uuid),
    };
    let mut conn = state.database.pool().acquire().await?;
    require_notifications(&mut conn, &caller).await?;
    let rows = notifications::deliveries(
        &mut conn,
        caller.organization_id,
        &filter,
        after,
        i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?,
    )
    .await?;
    Ok(Json(
        page.finish(
            rows.iter()
                .map(|delivery| {
                    (
                        format!("{}|{}", delivery.created_at.timestamp_micros(), delivery.id),
                        delivery_document(delivery),
                    )
                })
                .collect(),
        )?,
    ))
}

/// `GET /api/v1/notifications/deliveries/{id}`.
pub(super) async fn get_delivery(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = id.map_err(|_| ApiError::invalid())?.0;
    let mut conn = state.database.pool().acquire().await?;
    require_notifications(&mut conn, &caller).await?;
    let delivery = notifications::delivery(&mut conn, caller.organization_id, id).await?;
    let attempts = notifications::attempts(&mut conn, caller.organization_id, id).await?;
    let mut document = delivery_document(&delivery);
    document["attempts"] = Value::Array(
        attempts
            .iter()
            .map(|attempt| {
                json!({
                    "attempt": attempt.attempt,
                    "finished_at": attempt.finished_at,
                    "outcome": attempt.outcome.as_str().to_ascii_lowercase(),
                    "status": attempt.status,
                    "reason": attempt.reason,
                })
            })
            .collect(),
    );
    Ok(Json(document))
}

/// `GET /api/v1/notifications/destinations`.
pub(super) async fn list(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<ListDestinations>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let page = Page::parse(
        PageQuery::new(query.limit, query.cursor),
        caller.organization_id,
        caller.principal_id,
        "notifications-destinations",
    )?;
    let after = page
        .after
        .as_deref()
        .map(|after| {
            Uuid::parse_str(after)
                .map(NotificationDestinationId::from_uuid)
                .map_err(|_| ApiError::invalid())
        })
        .transpose()?;
    let mut conn = state.database.pool().acquire().await?;
    require_notifications(&mut conn, &caller).await?;
    let rows = notifications::list(
        &mut conn,
        caller.organization_id,
        after,
        i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?,
    )
    .await?;
    Ok(Json(
        page.finish(
            rows.iter()
                .map(|destination| (destination.id.to_string(), document(destination)))
                .collect(),
        )?,
    ))
}

/// `POST /api/v1/notifications/destinations`.
pub(super) async fn create(
    State(state): State<ApiState>,
    caller: Caller,
    body: Result<Json<CreateDestination>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let body = json_body(body)?;
    let mut conn = state.database.pool().acquire().await?;
    require_notifications(&mut conn, &caller).await?;
    // The authorization connection is released before a transaction is taken,
    // so one request never holds two pool connections.
    drop(conn);
    let name = validate_name(&body.name)?;
    let kind = DestinationKind::from_name(body.kind.trim()).ok_or_else(ApiError::invalid)?;
    let url_ref = validate_url_ref(&state, body.url_ref.trim()).await?;
    let signing_secret_ref = validate_secret_ref(kind, body.signing_secret_ref.as_deref())?;
    let mut tx = state.database.pool().begin().await?;
    let id = NotificationDestinationId::generate();
    let created = notifications::create(
        &mut tx,
        &NewNotificationDestination {
            id,
            organization_id: caller.organization_id,
            name,
            kind,
            url_ref,
            signing_secret_ref,
            enabled: body.enabled.unwrap_or(true),
            created_by: caller.principal_id,
        },
    )
    .await?;
    record(
        &mut tx,
        &caller,
        AuditAction::NotificationDestinationCreated,
        "notification_destination",
        &id.to_string(),
        AuditOutcome::Allowed,
        json!({
            "name": created.name,
            "kind": created.kind.as_str(),
            "url_ref": created.url_ref,
            "signing_secret_ref": created.signing_secret_ref,
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
            format!("/api/v1/notifications/destinations/{id}"),
        )],
        Json(document(&created)),
    )
        .into_response())
}

/// `GET /api/v1/notifications/destinations/{id}`.
pub(super) async fn get(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = destination_id(id)?;
    let mut conn = state.database.pool().acquire().await?;
    require_notifications(&mut conn, &caller).await?;
    let destination = notifications::destination(&mut conn, caller.organization_id, id).await?;
    Ok(Json(document(&destination)))
}

/// `PATCH /api/v1/notifications/destinations/{id}`.
pub(super) async fn update(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
    body: Result<Json<UpdateDestination>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = destination_id(id)?;
    let body = json_body(body)?;
    if body.revision < 1 {
        return Err(ApiError::invalid());
    }
    let mut conn = state.database.pool().acquire().await?;
    require_notifications(&mut conn, &caller).await?;
    let current = notifications::destination(&mut conn, caller.organization_id, id).await?;
    // The authorization connection is released before a transaction is taken,
    // so one request never holds two pool connections.
    drop(conn);
    let name = body.name.as_deref().map(validate_name).transpose()?;
    let url_ref = match body.url_ref.as_deref() {
        Some(raw) => Some(validate_url_ref(&state, raw.trim()).await?),
        None => None,
    };
    let signing_secret_ref = match body.signing_secret_ref.as_deref() {
        None => None,
        Some(reference) => {
            if !current.kind.requires_signing_secret() {
                return Err(ApiError::invalid());
            }
            SecretRef::parse(reference)
                .map(|reference| Some(reference.as_str().to_owned()))
                .map_err(|_| ApiError::invalid())?
        }
    };
    let mut tx = state.database.pool().begin().await?;
    let updated = notifications::update(
        &mut tx,
        caller.organization_id,
        id,
        &NotificationDestinationUpdate {
            name,
            url_ref,
            signing_secret_ref,
            enabled: body.enabled,
            expected_revision: body.revision,
        },
    )
    .await?;
    record(
        &mut tx,
        &caller,
        AuditAction::NotificationDestinationUpdated,
        "notification_destination",
        &id.to_string(),
        AuditOutcome::Allowed,
        json!({
            "name": updated.name,
            "kind": updated.kind.as_str(),
            "url_ref": updated.url_ref,
            "signing_secret_ref": updated.signing_secret_ref,
            "enabled": updated.enabled,
            "revision": updated.revision,
        }),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(document(&updated)))
}

/// `POST /api/v1/notifications/destinations/{id}/test`.
///
/// Sends one event with kind `notification.test` and reports the destination's
/// HTTP status. A non-2xx status is a completed test with `delivered: false`;
/// an error response means the request could not be made at all.
pub(super) async fn test(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = destination_id(id)?;
    let mut conn = state.database.pool().acquire().await?;
    require_notifications(&mut conn, &caller).await?;
    let destination = notifications::destination(&mut conn, caller.organization_id, id).await?;
    // Release the connection before the request, so no pool slot waits on HTTP.
    drop(conn);
    let event = Event {
        id: Uuid::now_v7(),
        kind: "notification.test".to_owned(),
        occurred_at: Utc::now(),
        organization_id: caller.organization_id,
        summary: "Bergsmith test notification".to_owned(),
        resource: ResourceRef {
            kind: "notification_destination".to_owned(),
            id: destination.id.to_string(),
            label: Some(destination.name.clone()),
        },
        reason: None,
    };
    let attempt = send_test(&state, &destination, &event).await;

    // Every attempt is audited, including one that could not be made: the
    // receiver may already have accepted a request whose response was lost.
    let mut conn = state.database.pool().acquire().await?;
    match &attempt {
        Ok(delivery) => {
            let detail = if delivery.delivered {
                json!({"status": delivery.status, "delivered": true})
            } else {
                json!({
                    "status": delivery.status,
                    "delivered": false,
                    "reason": "destination_rejected",
                })
            };
            let outcome = if delivery.delivered {
                AuditOutcome::Allowed
            } else {
                AuditOutcome::Failed
            };
            record(
                &mut conn,
                &caller,
                AuditAction::NotificationDestinationTested,
                "notification_destination",
                &id.to_string(),
                outcome,
                detail,
            )
            .await?;
        }
        Err(error) => {
            record(
                &mut conn,
                &caller,
                AuditAction::NotificationDestinationTested,
                "notification_destination",
                &id.to_string(),
                AuditOutcome::Failed,
                json!({
                    "delivered": false,
                    "reason": error.reason,
                    "operator_action": error.operator_action,
                }),
            )
            .await?;
        }
    }

    let delivery = attempt?;
    Ok(Json(json!({
        "delivered": delivery.delivered,
        "status": delivery.status,
        "response": delivery.response,
    })))
}

/// Resolves the destination URL reference and sends one event.
async fn send_test(
    state: &ApiState,
    destination: &NotificationDestination,
    event: &Event,
) -> Result<Delivery, ApiError> {
    let url = resolved_url(state, &destination.url_ref).await?;
    let target = Target {
        kind: destination.kind,
        url: &url,
        signing_secret_ref: destination.signing_secret_ref.as_deref(),
    };
    state
        .notifications
        .send(&target, event)
        .await
        .map_err(|error| delivery_error(&error))
}

fn delivery_error(error: &DeliveryError) -> ApiError {
    match error {
        DeliveryError::InvalidUrl(DestinationUrlError::NotAllowed) => ApiError::permanent(
            StatusCode::BAD_REQUEST,
            "destination_not_allowed",
            "add_the_host_to_notifications_allowed_hosts",
        ),
        DeliveryError::InvalidUrl(_) | DeliveryError::MissingSecret => ApiError::invalid(),
        DeliveryError::Secret(_) => ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "signing_secret_unavailable",
            "configure_the_signing_secret_file",
        ),
        DeliveryError::Encode(_) | DeliveryError::Client { .. } => ApiError::internal(),
        DeliveryError::Transport { .. } => ApiError::permanent(
            StatusCode::BAD_GATEWAY,
            "notification_delivery_failed",
            "check_the_destination_and_network",
        ),
    }
}

pub(super) async fn record(
    conn: &mut sqlx::PgConnection,
    caller: &Caller,
    action: AuditAction,
    resource_type: &str,
    resource_id: &str,
    outcome: AuditOutcome,
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
            outcome,
            detail,
        },
    )
    .await?;
    Ok(())
}
