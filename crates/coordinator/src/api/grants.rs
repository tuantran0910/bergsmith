//! Scoped role-grant administration (BS-023.T2).
//!
//! | Route | Purpose |
//! |---|---|
//! | `GET /api/v1/grants` | Page through active grants in a scope |
//! | `POST /api/v1/grants` | Grant a role over the organization, a connection, or a table |
//! | `DELETE /api/v1/grants/{id}` | Revoke an active grant |
//!
//! A caller must hold `grants:manage` over the scope it reads or changes. That
//! is the scope limit: an organization administrator manages every scope, a
//! connection administrator manages that connection and its tables, and a
//! table administrator manages that table. Grants are immutable except for
//! revocation, which sets `revoked_at` and keeps the row for audit.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bergsmith_common::ids::{CatalogConnectionId, PrincipalId, RoleGrantId, TableId};
use bergsmith_common::permission::{GrantScope, Permission, Role};
use bergsmith_persistence::grants::{self, GrantFilter, NewRoleGrant, RoleGrant};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::identity::administration;

use super::pagination::{Page, PageQuery};
use super::{ApiError, ApiState, Caller, json_body, resources};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListGrants {
    principal_id: Option<Uuid>,
    catalog_connection: Option<Uuid>,
    table: Option<Uuid>,
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateGrant {
    principal_id: Uuid,
    role: String,
    #[serde(default)]
    scope: GrantScope,
}

/// The caller-visible shape of a grant. It never carries a credential.
fn document(grant: &RoleGrant) -> Value {
    json!({
        "id": grant.id,
        "principal_id": grant.principal_id,
        "role": grant.role.as_str(),
        "scope": grant.scope,
        "granted_by": grant.granted_by,
        "granted_at": grant.granted_at,
    })
}

/// The one scope a grant page is filtered by.
fn filter_scope(filter: GrantFilter) -> GrantScope {
    match (filter.catalog_connection, filter.table) {
        (_, Some(table)) => GrantScope::table(table),
        (Some(connection), None) => GrantScope::connection(connection),
        (None, None) => GrantScope::organization(),
    }
}

pub(super) async fn list(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<ListGrants>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    if query.catalog_connection.is_some() && query.table.is_some() {
        // A page is filtered by exactly one scope, matching how a grant names
        // one.
        return Err(ApiError::invalid());
    }
    let filter = GrantFilter {
        principal_id: query.principal_id.map(PrincipalId::from_uuid),
        catalog_connection: query.catalog_connection.map(CatalogConnectionId::from_uuid),
        table: query.table.map(TableId::from_uuid),
    };
    // The cursor is bound to the filters as well as the resource, so a caller
    // cannot widen or narrow a page midway through it.
    let resource = format!(
        "grants:{}:{}:{}",
        filter
            .principal_id
            .map(|principal| principal.to_string())
            .unwrap_or_default(),
        filter
            .catalog_connection
            .map(|connection| connection.to_string())
            .unwrap_or_default(),
        filter
            .table
            .map(|table| table.to_string())
            .unwrap_or_default(),
    );
    let page = Page::parse(
        PageQuery::new(query.limit, query.cursor),
        caller.organization_id,
        caller.principal_id,
        resource,
    )?;
    let after = page
        .after
        .as_deref()
        .map(|after| {
            Uuid::parse_str(after)
                .map(RoleGrantId::from_uuid)
                .map_err(|_| ApiError::invalid())
        })
        .transpose()?;
    let mut conn = state.database.pool().acquire().await?;
    resources::require(
        &mut conn,
        &caller.0,
        Permission::ManageGrants,
        filter_scope(filter).resource(),
    )
    .await?;
    let rows = grants::list_grants(
        &mut conn,
        caller.organization_id,
        filter,
        after,
        i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?,
    )
    .await?;
    Ok(Json(
        page.finish(
            rows.iter()
                .map(|grant| (grant.id.to_string(), document(grant)))
                .collect(),
        )?,
    ))
}

pub(super) async fn create(
    State(state): State<ApiState>,
    caller: Caller,
    body: Result<Json<CreateGrant>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let body = json_body(body)?;
    let role = Role::from_name(&body.role).ok_or_else(ApiError::invalid)?;
    if body.scope.catalog_connection.is_some() && body.scope.table.is_some() {
        return Err(ApiError::invalid());
    }
    let scope = body.scope.resource();
    let mut conn = state.database.pool().acquire().await?;
    // The denial is recorded here; the administration function rechecks the
    // permission inside the transaction that writes the grant and its audit
    // row, so a revocation that lands in between still wins.
    resources::require(&mut conn, &caller.0, Permission::ManageGrants, scope).await?;
    let new = NewRoleGrant {
        id: RoleGrantId::generate(),
        organization_id: caller.organization_id,
        principal_id: PrincipalId::from_uuid(body.principal_id),
        role,
        scope: body.scope,
        granted_by: Some(caller.principal_id),
    };
    let granted =
        administration::grant_role(&mut conn, caller.organization_id, caller.principal_id, &new)
            .await;
    let created =
        resources::administered(&mut conn, &caller, Permission::ManageGrants, scope, granted)
            .await?;
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, format!("/api/v1/grants/{}", created.id))],
        Json(document(&created)),
    )
        .into_response())
}

pub(super) async fn revoke(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = RoleGrantId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    let mut conn = state.database.pool().acquire().await?;
    // Loading the grant first names the scope the revocation actually covers;
    // the transactional path re-reads it under the lifecycle lock, so a grant
    // revoked in between is a 404 rather than a stale success.
    let existing = grants::active_grant(&mut conn, caller.organization_id, id).await?;
    let scope = existing.scope.resource();
    resources::require(&mut conn, &caller.0, Permission::ManageGrants, scope).await?;
    let revoked =
        administration::revoke_role(&mut conn, caller.organization_id, caller.principal_id, id)
            .await;
    let revoked =
        resources::administered(&mut conn, &caller, Permission::ManageGrants, scope, revoked)
            .await?;
    Ok(Json(document(&revoked)))
}
