//! Principal administration (BS-023.T1).
//!
//! | Route | Purpose |
//! |---|---|
//! | `GET /api/v1/principals` | Page through the organization's users and services |
//! | `POST /api/v1/principals` | Provision a user for an issuer and subject |
//! | `GET /api/v1/principals/{id}` | One principal |
//! | `PATCH /api/v1/principals/{id}` | Deactivate or reactivate a principal |
//!
//! Every route requires `grants:manage` over the organization: a principal's
//! issuer and subject are the key sign-in is bound to, so reading or changing
//! them is access administration. Deactivation is a lifecycle change, not a
//! deletion: the row, its grants, and its history stay, its sessions end, and
//! the authorization check every command runs stops applying its grants.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bergsmith_common::ids::PrincipalId;
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::tenancy::{self, NewPrincipal, Principal, PrincipalKind};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::identity::administration;

use super::pagination::{Page, PageQuery};
use super::{ApiError, ApiState, Caller, json_body, resources};

/// The longest issuer or subject the API stores, in bytes.
const MAX_IDENTITY: usize = 2048;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListPrincipals {
    kind: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreatePrincipal {
    issuer: String,
    subject: String,
    display_name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdatePrincipal {
    active: bool,
}

/// The caller-visible shape of a principal. It never carries a credential:
/// a service account's token is shown once when it is created (BS-023.T4).
fn document(principal: &Principal) -> Value {
    json!({
        "id": principal.id,
        "kind": principal.kind.as_str(),
        "issuer": principal.issuer,
        "subject": principal.subject,
        "display_name": principal.display_name,
        "active": principal.is_active(),
        "created_at": principal.created_at,
        "deactivated_at": principal.deactivated_at,
    })
}

pub(super) async fn list(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<ListPrincipals>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let kind = match query.kind.as_deref() {
        None => None,
        Some("User") => Some(PrincipalKind::User),
        Some("Service") => Some(PrincipalKind::Service),
        Some(_) => return Err(ApiError::invalid()),
    };
    let page = Page::parse(
        PageQuery::new(query.limit, query.cursor),
        caller.organization_id,
        caller.principal_id,
        "principals",
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
    resources::require(
        &mut conn,
        &caller.0,
        Permission::ManageGrants,
        ResourceScope::Organization,
    )
    .await?;
    let rows = tenancy::list_principals(
        &mut conn,
        caller.organization_id,
        kind,
        after,
        page.limit + 1,
    )
    .await?;
    Ok(Json(
        page.finish(
            rows.iter()
                .map(|principal| (principal.id.to_string(), document(principal)))
                .collect(),
        )?,
    ))
}

pub(super) async fn create(
    State(state): State<ApiState>,
    caller: Caller,
    body: Result<Json<CreatePrincipal>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let body = json_body(body)?;
    let issuer = body.issuer.trim();
    let subject = body.subject.trim();
    let display_name = body.display_name.trim();
    if issuer.is_empty()
        || issuer.len() > MAX_IDENTITY
        || subject.is_empty()
        || subject.len() > MAX_IDENTITY
        || display_name.is_empty()
        || display_name.chars().count() > 200
    {
        return Err(ApiError::invalid());
    }
    let mut conn = state.database.pool().acquire().await?;
    // The denial is recorded here; the administration function rechecks the
    // permission inside the transaction that writes the principal and its
    // audit row, so a revocation that lands in between still wins.
    resources::require(
        &mut conn,
        &caller.0,
        Permission::ManageGrants,
        ResourceScope::Organization,
    )
    .await?;
    let new = NewPrincipal {
        id: PrincipalId::generate(),
        organization_id: caller.organization_id,
        kind: PrincipalKind::User,
        issuer: issuer.to_owned(),
        subject: subject.to_owned(),
        display_name: display_name.to_owned(),
    };
    let provisioned = administration::provision_principal(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        &new,
    )
    .await;
    let created = resources::administered(
        &mut conn,
        &caller,
        Permission::ManageGrants,
        ResourceScope::Organization,
        provisioned,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        [(
            header::LOCATION,
            format!("/api/v1/principals/{}", created.id),
        )],
        Json(document(&created)),
    )
        .into_response())
}

pub(super) async fn get(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = PrincipalId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    let mut conn = state.database.pool().acquire().await?;
    resources::require(
        &mut conn,
        &caller.0,
        Permission::ManageGrants,
        ResourceScope::Organization,
    )
    .await?;
    let principal = tenancy::principal(&mut conn, caller.organization_id, id).await?;
    Ok(Json(document(&principal)))
}

pub(super) async fn update(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
    body: Result<Json<UpdatePrincipal>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = PrincipalId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    let body = json_body(body)?;
    let mut conn = state.database.pool().acquire().await?;
    resources::require(
        &mut conn,
        &caller.0,
        Permission::ManageGrants,
        ResourceScope::Organization,
    )
    .await?;
    let principal = if body.active {
        let reactivated = administration::reactivate_principal(
            &mut conn,
            caller.organization_id,
            caller.principal_id,
            id,
        )
        .await;
        resources::administered(
            &mut conn,
            &caller,
            Permission::ManageGrants,
            ResourceScope::Organization,
            reactivated,
        )
        .await?
    } else {
        let deactivated = administration::deactivate_principal(
            &mut conn,
            caller.organization_id,
            caller.principal_id,
            id,
        )
        .await;
        resources::administered(
            &mut conn,
            &caller,
            Permission::ManageGrants,
            ResourceScope::Organization,
            deactivated,
        )
        .await?
        .0
    };
    Ok(Json(document(&principal)))
}
