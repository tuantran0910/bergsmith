//! Service identities and worker pool tokens (BS-023.T4).
//!
//! | Route | Purpose |
//! |---|---|
//! | `GET /api/v1/service-identities` | Page through service principals |
//! | `POST /api/v1/service-identities` | Create a service principal |
//! | `GET /api/v1/service-identities/{id}` | One service principal |
//! | `GET /api/v1/service-identities/{id}/tokens` | Page through its pool tokens |
//! | `POST /api/v1/service-identities/{id}/tokens` | Issue a token, shown once |
//! | `GET /api/v1/service-identities/{id}/tokens/{token}` | Token metadata |
//! | `POST /api/v1/service-identities/{id}/tokens/{token}/rotate` | Replace and revoke |
//! | `DELETE /api/v1/service-identities/{id}/tokens/{token}` | Revoke |
//!
//! Every route requires `workers:manage` over the organization. A bearer token
//! appears only in the issue and rotate responses; listings carry metadata and
//! never the verifier or the secret.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bergsmith_common::ids::{PrincipalId, WorkerPoolTokenId};
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::tenancy;
use bergsmith_persistence::worker_tokens::{self, WorkerPoolToken};
use bergsmith_protocol::identity::valid_pool;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::identity::administration;

use super::pagination::{Page, PageQuery};
use super::{ApiError, ApiState, Caller, json_body, principals, resources};

/// The longest service identity display name, in characters.
const MAX_DISPLAY_NAME: usize = 200;

/// The longest token label, in characters.
const MAX_LABEL: usize = 200;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListServiceIdentities {
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateServiceIdentity {
    display_name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IssueWorkerToken {
    pool: String,
    label: Option<String>,
}

fn token_document(token: &WorkerPoolToken) -> Value {
    json!({
        "id": token.id,
        "principal_id": token.principal_id,
        "pool": token.pool,
        "label": token.label,
        "created_by": token.created_by,
        "created_at": token.created_at,
        "rotated_from": token.rotated_from,
        "revoked_by": token.revoked_by,
        "revoked_at": token.revoked_at,
        "active": token.is_active(),
    })
}

fn issued_document(token: &WorkerPoolToken, secret: &str) -> Value {
    let mut document = token_document(token);
    if let Some(object) = document.as_object_mut() {
        object.insert("token".to_owned(), Value::String(secret.to_owned()));
    }
    document
}

fn validate_pool(pool: &str) -> Result<(), ApiError> {
    if valid_pool(pool.trim()) {
        Ok(())
    } else {
        Err(ApiError::invalid())
    }
}

fn validate_label(label: Option<&str>) -> Result<Option<String>, ApiError> {
    match label {
        None => Ok(None),
        Some(label) => {
            let label = label.trim();
            if label.is_empty() || label.chars().count() > MAX_LABEL {
                return Err(ApiError::invalid());
            }
            Ok(Some(label.to_owned()))
        }
    }
}

async fn require_workers(conn: &mut sqlx::PgConnection, caller: &Caller) -> Result<(), ApiError> {
    resources::require(
        conn,
        &caller.0,
        Permission::ManageWorkers,
        ResourceScope::Organization,
    )
    .await
}

fn principal_id(
    path: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<PrincipalId, ApiError> {
    Ok(PrincipalId::from_uuid(
        path.map_err(|_| ApiError::invalid())?.0,
    ))
}

pub(super) async fn list(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<ListServiceIdentities>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let page = Page::parse(
        PageQuery::new(query.limit, query.cursor),
        caller.organization_id,
        caller.principal_id,
        "service-identities",
    )?;
    let after = page
        .after
        .as_deref()
        .map(|after| {
            Uuid::parse_str(after)
                .map(PrincipalId::from_uuid)
                .map_err(|_| ApiError::invalid())
        })
        .transpose()?;
    let mut conn = state.database.pool().acquire().await?;
    require_workers(&mut conn, &caller).await?;
    let rows = tenancy::list_principals(
        &mut conn,
        caller.organization_id,
        Some(tenancy::PrincipalKind::Service),
        after,
        page.limit + 1,
    )
    .await?;
    Ok(Json(
        page.finish(
            rows.iter()
                .map(|principal| (principal.id.to_string(), principals::document(principal)))
                .collect(),
        )?,
    ))
}

pub(super) async fn create(
    State(state): State<ApiState>,
    caller: Caller,
    body: Result<Json<CreateServiceIdentity>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let body = json_body(body)?;
    let display_name = body.display_name.trim();
    if display_name.is_empty() || display_name.chars().count() > MAX_DISPLAY_NAME {
        return Err(ApiError::invalid());
    }
    let mut conn = state.database.pool().acquire().await?;
    require_workers(&mut conn, &caller).await?;
    let created = administration::create_service_identity(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        display_name,
    )
    .await;
    let created = resources::administered(
        &mut conn,
        &caller,
        Permission::ManageWorkers,
        ResourceScope::Organization,
        created,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        [(
            header::LOCATION,
            format!("/api/v1/service-identities/{}", created.id),
        )],
        Json(principals::document(&created)),
    )
        .into_response())
}

pub(super) async fn get(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = principal_id(id)?;
    let mut conn = state.database.pool().acquire().await?;
    require_workers(&mut conn, &caller).await?;
    let principal = tenancy::principal(&mut conn, caller.organization_id, id).await?;
    if principal.kind != tenancy::PrincipalKind::Service {
        return Err(ApiError::not_found());
    }
    Ok(Json(principals::document(&principal)))
}

pub(super) async fn list_tokens(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
    query: Result<Query<ListServiceIdentities>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let principal_id = principal_id(id)?;
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let page = Page::parse(
        PageQuery::new(query.limit, query.cursor),
        caller.organization_id,
        caller.principal_id,
        format!("service-identities:{principal_id}:tokens"),
    )?;
    let after = page
        .after
        .as_deref()
        .map(|after| {
            Uuid::parse_str(after)
                .map(WorkerPoolTokenId::from_uuid)
                .map_err(|_| ApiError::invalid())
        })
        .transpose()?;
    let mut conn = state.database.pool().acquire().await?;
    require_workers(&mut conn, &caller).await?;
    let principal = tenancy::principal(&mut conn, caller.organization_id, principal_id).await?;
    if principal.kind != tenancy::PrincipalKind::Service {
        return Err(ApiError::not_found());
    }
    let rows = worker_tokens::list(
        &mut conn,
        caller.organization_id,
        principal_id,
        after,
        i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?,
    )
    .await?;
    Ok(Json(
        page.finish(
            rows.iter()
                .map(|token| (token.id.to_string(), token_document(token)))
                .collect(),
        )?,
    ))
}

pub(super) async fn get_token(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<(Uuid, Uuid)>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let (principal_id, token_id) = id.map_err(|_| ApiError::invalid())?.0;
    let mut conn = state.database.pool().acquire().await?;
    require_workers(&mut conn, &caller).await?;
    let token = worker_tokens::token(
        &mut conn,
        caller.organization_id,
        PrincipalId::from_uuid(principal_id),
        WorkerPoolTokenId::from_uuid(token_id),
    )
    .await?;
    Ok(Json(token_document(&token)))
}

pub(super) async fn issue(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
    body: Result<Json<IssueWorkerToken>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let principal_id = principal_id(id)?;
    let body = json_body(body)?;
    let pool = body.pool.trim();
    validate_pool(pool)?;
    let label = validate_label(body.label.as_deref())?;
    let mut conn = state.database.pool().acquire().await?;
    require_workers(&mut conn, &caller).await?;
    let issued = administration::issue_worker_token(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        principal_id,
        pool,
        label.as_deref(),
    )
    .await;
    let issued = resources::administered(
        &mut conn,
        &caller,
        Permission::ManageWorkers,
        ResourceScope::Organization,
        issued,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        [(
            header::LOCATION,
            format!(
                "/api/v1/service-identities/{principal_id}/tokens/{}",
                issued.token.id
            ),
        )],
        Json(issued_document(&issued.token, issued.secret())),
    )
        .into_response())
}

pub(super) async fn rotate(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<(Uuid, Uuid)>, axum::extract::rejection::PathRejection>,
) -> Result<Response, ApiError> {
    let (principal_id, token_id) = id.map_err(|_| ApiError::invalid())?.0;
    let principal_id = PrincipalId::from_uuid(principal_id);
    let token_id = WorkerPoolTokenId::from_uuid(token_id);
    let mut conn = state.database.pool().acquire().await?;
    require_workers(&mut conn, &caller).await?;
    let issued = administration::rotate_worker_token(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        principal_id,
        token_id,
    )
    .await;
    let issued = resources::administered(
        &mut conn,
        &caller,
        Permission::ManageWorkers,
        ResourceScope::Organization,
        issued,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        [(
            header::LOCATION,
            format!(
                "/api/v1/service-identities/{principal_id}/tokens/{}",
                issued.token.id
            ),
        )],
        Json(issued_document(&issued.token, issued.secret())),
    )
        .into_response())
}

pub(super) async fn revoke(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<(Uuid, Uuid)>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let (principal_id, token_id) = id.map_err(|_| ApiError::invalid())?.0;
    let principal_id = PrincipalId::from_uuid(principal_id);
    let token_id = WorkerPoolTokenId::from_uuid(token_id);
    let mut conn = state.database.pool().acquire().await?;
    require_workers(&mut conn, &caller).await?;
    let revoked = administration::revoke_worker_token(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        principal_id,
        token_id,
    )
    .await;
    let revoked = resources::administered(
        &mut conn,
        &caller,
        Permission::ManageWorkers,
        ResourceScope::Organization,
        revoked,
    )
    .await?;
    Ok(Json(token_document(&revoked)))
}
