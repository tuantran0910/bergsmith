//! The session's saved list views (BS-033.T6, ADR 0067).
//!
//! `/me/views` belongs to the signed-in person: every read and write is
//! scoped to their principal in their organization, so no grant is needed
//! and another person's view reads as missing. A view holds a list's own
//! search parameters, checked against that list's names and bounded in size
//! before it is written. Saving takes an idempotency key; edits and deletes
//! name the revision they read. Every write is audited in its transaction.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use bergsmith_common::audit::{ActorKind, AuditAction, AuditOutcome};
use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::SavedViewId;
use bergsmith_common::views::{ViewError, ViewPage, ViewParams, view_name};
use bergsmith_persistence::audit::NewAuditEvent;
use bergsmith_persistence::idempotency::{self, IdempotencyKey, IdempotencyStatus, RecordOutcome};
use bergsmith_persistence::saved_views::{self, NewSavedView, SavedView, SavedViewUpdate};
use bergsmith_persistence::{PersistenceError, audit};
use chrono::SecondsFormat;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, ApiState, Caller, json_body};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListViews {
    page: Option<String>,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateViewRequest {
    page: String,
    name: String,
    params: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdateViewRequest {
    revision: i64,
    name: Option<String>,
    params: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeleteViewQuery {
    revision: i64,
}

fn page(value: &str) -> Result<ViewPage, ApiError> {
    value.parse().map_err(|_| ApiError::invalid())
}

fn view_error(_error: ViewError) -> ApiError {
    ApiError::invalid()
}

fn view_id(
    id: Result<Path<uuid::Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<SavedViewId, ApiError> {
    Ok(SavedViewId::from_uuid(
        id.map_err(|_| ApiError::invalid())?.0,
    ))
}

fn view_json(view: &SavedView) -> Value {
    json!({
        "id": view.id.to_string(),
        "page": view.page.as_str(),
        "name": view.name,
        "params": view.params,
        "revision": view.revision,
        "created_at": view.created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        "updated_at": view.updated_at.to_rfc3339_opts(SecondsFormat::Millis, true),
    })
}

fn saved_view_error(error: PersistenceError) -> ApiError {
    match error {
        PersistenceError::LimitReached { .. } => {
            ApiError::conflict("view_limit_reached", "delete_a_saved_view")
        }
        other => other.into(),
    }
}

/// Binds a save to its actor and canonical request.
fn create_key(
    caller: &Caller,
    headers: &HeaderMap,
    request: &CreateViewRequest,
) -> Result<IdempotencyKey, ApiError> {
    let key = headers
        .get("idempotency-key")
        .and_then(|key| key.to_str().ok())
        .filter(|key| {
            !key.is_empty() && key.len() <= 255 && key.bytes().all(|b| b.is_ascii_graphic())
        })
        .ok_or_else(ApiError::invalid)?;
    // The actor is part of the hash: a key cannot reveal another person's view.
    let canonical =
        serde_json::to_vec(&(caller.principal_id, request)).map_err(|_| ApiError::invalid())?;
    Ok(IdempotencyKey {
        organization_id: caller.organization_id,
        key: key.into(),
        operation: "SaveView".into(),
        request_hash: Sha256Digest::of(b"bergsmith.api.save-view.v1", &canonical).to_hex(),
    })
}

/// Appends one audit record for a change to the caller's view.
async fn record(
    conn: &mut sqlx::PgConnection,
    caller: &Caller,
    action: AuditAction,
    view: &SavedView,
) -> Result<(), ApiError> {
    audit::record(
        conn,
        &NewAuditEvent {
            organization_id: Some(caller.organization_id),
            actor_principal_id: Some(caller.principal_id),
            actor_kind: ActorKind::User,
            action: action.as_str(),
            resource_type: "saved_view",
            resource_id: Some(&view.id.to_string()),
            outcome: AuditOutcome::Allowed,
            // Names and parameters are the person's own; the record keeps
            // which list and revision changed, not their text.
            detail: json!({ "page": view.page.as_str(), "revision": view.revision }),
        },
    )
    .await?;
    Ok(())
}

/// Lists the caller's views, of one list when `page` is given.
pub(super) async fn list(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<ListViews>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let page = query.page.as_deref().map(page).transpose()?;
    let mut conn = state.database.pool().acquire().await?;
    let views =
        saved_views::list(&mut conn, caller.organization_id, caller.principal_id, page).await?;
    Ok(Json(json!({
        "items": views.iter().map(view_json).collect::<Vec<_>>(),
    })))
}

/// Saves a view; replaying the same key and request returns the same view.
pub(super) async fn create(
    State(state): State<ApiState>,
    caller: Caller,
    headers: HeaderMap,
    body: Result<Json<CreateViewRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let request = json_body(body)?;
    let key = create_key(&caller, &headers, &request)?;
    let page = page(&request.page)?;
    let name = view_name(&request.name).map_err(view_error)?;
    let params = ViewParams::parse(page, request.params).map_err(view_error)?;
    let mut tx = state.database.pool().begin().await?;
    if let IdempotencyStatus::Replay { resource_id } = idempotency::check(&mut tx, &key).await? {
        tx.rollback().await?;
        return replayed(&state, &caller, resource_id).await;
    }
    let id = SavedViewId::generate();
    let created = saved_views::create(
        &mut tx,
        &NewSavedView {
            id,
            organization_id: caller.organization_id,
            principal_id: caller.principal_id,
            page,
            name: &name,
            params: &params,
        },
    )
    .await
    .map_err(saved_view_error)?;
    if let RecordOutcome::Replay { resource_id } =
        idempotency::record(&mut tx, &key, *id.as_uuid()).await?
    {
        // A concurrent identical save won; discard this one.
        tx.rollback().await?;
        return replayed(&state, &caller, resource_id).await;
    }
    record(&mut tx, &caller, AuditAction::SavedViewCreated, &created).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(view_json(&created))))
}

/// The view an earlier identical save created.
async fn replayed(
    state: &ApiState,
    caller: &Caller,
    resource_id: uuid::Uuid,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let mut conn = state.database.pool().acquire().await?;
    let view = saved_views::get(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        SavedViewId::from_uuid(resource_id),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(view_json(&view))))
}

/// Renames a view or replaces its parameters when the revision still matches.
pub(super) async fn update(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<uuid::Uuid>, axum::extract::rejection::PathRejection>,
    body: Result<Json<UpdateViewRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = view_id(id)?;
    let request = json_body(body)?;
    if request.revision < 1 || (request.name.is_none() && request.params.is_none()) {
        return Err(ApiError::invalid());
    }
    let name = request
        .name
        .as_deref()
        .map(view_name)
        .transpose()
        .map_err(view_error)?;
    let mut tx = state.database.pool().begin().await?;
    // Parameters are checked against the view's own list.
    let current =
        saved_views::get(&mut tx, caller.organization_id, caller.principal_id, id).await?;
    let params = request
        .params
        .map(|params| ViewParams::parse(current.page, params))
        .transpose()
        .map_err(view_error)?;
    let updated = saved_views::update(
        &mut tx,
        caller.organization_id,
        caller.principal_id,
        id,
        &SavedViewUpdate {
            name: name.as_deref(),
            params: params.as_ref(),
            expected_revision: request.revision,
        },
    )
    .await?;
    record(&mut tx, &caller, AuditAction::SavedViewUpdated, &updated).await?;
    tx.commit().await?;
    Ok(Json(view_json(&updated)))
}

/// Deletes a view when the revision still matches, returning what it held.
pub(super) async fn delete(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<uuid::Uuid>, axum::extract::rejection::PathRejection>,
    query: Result<Query<DeleteViewQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = view_id(id)?;
    let revision = query.map_err(|_| ApiError::invalid())?.0.revision;
    if revision < 1 {
        return Err(ApiError::invalid());
    }
    let mut tx = state.database.pool().begin().await?;
    let deleted = saved_views::delete(
        &mut tx,
        caller.organization_id,
        caller.principal_id,
        id,
        revision,
    )
    .await?;
    record(&mut tx, &caller, AuditAction::SavedViewDeleted, &deleted).await?;
    tx.commit().await?;
    Ok(Json(view_json(&deleted)))
}
