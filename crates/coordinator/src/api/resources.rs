use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::grants;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::pagination::{Page, PageQuery};
use super::{ApiError, ApiState, Caller};

// Queries are compile-time literals. All request values are bound.
// Filtering precedes ORDER BY / LIMIT, so inaccessible rows never consume a page.
macro_rules! table_access {
    () => {
        "EXISTS (
    SELECT 1 FROM principal_role_grants g
    WHERE g.organization_id = $1 AND g.principal_id = $2
      AND g.revoked_at IS NULL AND g.role = ANY($3::text[])
      AND ((g.catalog_connection_id IS NULL AND g.table_identity_id IS NULL)
        OR g.table_identity_id = t.id
        OR EXISTS (SELECT 1 FROM table_aliases a
                   WHERE a.organization_id = $1 AND a.table_identity_id = t.id
                     AND a.catalog_connection_id = g.catalog_connection_id)))"
    };
}

macro_rules! catalog_access {
    () => {
        "EXISTS (
    SELECT 1 FROM principal_role_grants g
    WHERE g.organization_id = $1 AND g.principal_id = $2
      AND g.revoked_at IS NULL AND g.role = ANY($3::text[])
      AND ((g.catalog_connection_id IS NULL AND g.table_identity_id IS NULL)
        OR g.catalog_connection_id = r.id))"
    };
}

macro_rules! table_summary {
    () => {
        "jsonb_build_object('id', r.id, 'table_uuid', r.table_uuid, 'created_at', r.created_at,
          'aliases', COALESCE((SELECT jsonb_agg(jsonb_build_object(
              'catalog_id', a.catalog_connection_id, 'namespace', a.namespace, 'name', a.table_name)
              ORDER BY a.catalog_connection_id, a.namespace, a.table_name)
            FROM table_aliases a WHERE a.organization_id = $1 AND a.table_identity_id = r.id
              AND EXISTS (SELECT 1 FROM principal_role_grants g
                WHERE g.organization_id = $1 AND g.principal_id = $2
                  AND g.revoked_at IS NULL AND g.role = ANY($3::text[])
                  AND ((g.catalog_connection_id IS NULL AND g.table_identity_id IS NULL)
                    OR g.table_identity_id = r.id
                    OR g.catalog_connection_id = a.catalog_connection_id))), '[]'::jsonb))"
    };
}

#[derive(Clone, Copy)]
enum Selection {
    Item,
    First,
    Continuation,
}

#[derive(Clone, Copy)]
enum Resource {
    Catalog,
    Table,
    Job,
}

impl Resource {
    const fn name(self) -> &'static str {
        match self {
            Self::Catalog => "catalogs",
            Self::Table => "tables",
            Self::Job => "jobs",
        }
    }
    const fn permission(self) -> Permission {
        match self {
            Self::Catalog | Self::Table => Permission::ViewInventory,
            Self::Job => Permission::ViewJobs,
        }
    }
    const fn query(self, selection: Selection) -> &'static str {
        macro_rules! query {
            ($from:literal, $projection:expr, $access:expr, $tail:literal) => {
                concat!(
                    "SELECT r.id, ",
                    $projection,
                    " AS document FROM ",
                    $from,
                    " WHERE r.organization_id = $1 AND ",
                    $access,
                    $tail
                )
            };
        }
        macro_rules! variants {
            ($from:literal, $projection:expr, $access:expr) => {
                match selection {
                    Selection::Item => query!(
                        $from,
                        $projection,
                        $access,
                        " AND r.id = $4 ORDER BY r.id LIMIT 1"
                    ),
                    Selection::First => {
                        query!($from, $projection, $access, " ORDER BY r.id LIMIT $4")
                    }
                    Selection::Continuation => query!(
                        $from,
                        $projection,
                        $access,
                        " AND r.id > $4 ORDER BY r.id LIMIT $5"
                    ),
                }
            };
        }
        match self {
            Self::Catalog => variants!(
                "catalog_connections r",
                "jsonb_build_object('id', r.id, 'name', r.name, 'created_at', r.created_at)",
                catalog_access!()
            ),
            Self::Table => variants!(
                "table_identities r JOIN table_identities t ON t.id = r.id",
                table_summary!(),
                table_access!()
            ),
            Self::Job => variants!(
                "jobs r JOIN table_identities t ON t.id = r.table_identity_id AND t.organization_id = r.organization_id",
                "jsonb_build_object('id', r.id, 'table_id', r.table_identity_id, 'state', r.state, 'revision', r.state_revision, 'state_reason', r.state_reason, 'retry_of_job_id', r.retry_of_job_id, 'created_by', r.created_by, 'created_at', r.created_at, 'updated_at', r.updated_at, 'cancellation_requested', r.cancellation_requested_at IS NOT NULL, 'publication_started', r.publication_started_at IS NOT NULL)",
                table_access!()
            ),
        }
    }
}

async fn read(
    state: &ApiState,
    caller: &bergsmith_persistence::auth::Session,
    resource: Resource,
    query: PageQuery,
    id: Option<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let page = Page::parse(
        query,
        caller.organization_id,
        caller.principal_id,
        resource.name(),
    )?;
    let after = page
        .after
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|_| ApiError::invalid())?;
    let roles: Vec<_> = resource
        .permission()
        .roles()
        .iter()
        .map(|role| role.as_str())
        .collect();
    let limit = i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?;
    // Each shape is a static query with a direct index predicate. A nullable
    // predicate here would let PostgreSQL's generic plan scan and discard rows.
    let rows: Vec<(Uuid, Value)> = if let Some(id) = id {
        sqlx::query_as(resource.query(Selection::Item))
            .bind(caller.organization_id.as_uuid())
            .bind(caller.principal_id.as_uuid())
            .bind(roles)
            .bind(id)
            .fetch_all(state.database.pool())
            .await?
    } else if let Some(after) = after {
        sqlx::query_as(resource.query(Selection::Continuation))
            .bind(caller.organization_id.as_uuid())
            .bind(caller.principal_id.as_uuid())
            .bind(roles)
            .bind(after)
            .bind(limit)
            .fetch_all(state.database.pool())
            .await?
    } else {
        sqlx::query_as(resource.query(Selection::First))
            .bind(caller.organization_id.as_uuid())
            .bind(caller.principal_id.as_uuid())
            .bind(roles)
            .bind(limit)
            .fetch_all(state.database.pool())
            .await?
    };
    if id.is_some() {
        return rows
            .into_iter()
            .next()
            .map(|(_, value)| Json(value))
            .ok_or_else(ApiError::not_found);
    }
    Ok(Json(
        page.finish(
            rows.into_iter()
                .map(|(id, value)| (id.to_string(), value))
                .collect(),
        )?,
    ))
}

pub(super) async fn catalogs(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<PageQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller.0,
        Resource::Catalog,
        query.map_err(|_| ApiError::invalid())?.0,
        None,
    )
    .await
}
pub(super) async fn tables(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<PageQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller.0,
        Resource::Table,
        query.map_err(|_| ApiError::invalid())?.0,
        None,
    )
    .await
}
pub(super) async fn jobs(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<PageQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller.0,
        Resource::Job,
        query.map_err(|_| ApiError::invalid())?.0,
        None,
    )
    .await
}
pub(super) async fn catalog(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller.0,
        Resource::Catalog,
        PageQuery::default(),
        Some(id.map_err(|_| ApiError::invalid())?.0),
    )
    .await
}
pub(super) async fn table(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller.0,
        Resource::Table,
        PageQuery::default(),
        Some(id.map_err(|_| ApiError::invalid())?.0),
    )
    .await
}
pub(super) async fn job(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller.0,
        Resource::Job,
        PageQuery::default(),
        Some(id.map_err(|_| ApiError::invalid())?.0),
    )
    .await
}

pub(super) async fn require(
    conn: &mut PgConnection,
    caller: &bergsmith_persistence::auth::Session,
    permission: Permission,
    resource: ResourceScope,
) -> Result<(), ApiError> {
    if let Err(error) = grants::require_permission(
        conn,
        caller.organization_id,
        caller.principal_id,
        permission,
        resource,
    )
    .await
    {
        if matches!(
            error,
            bergsmith_persistence::PersistenceError::PermissionDenied { .. }
        ) {
            record_denial(conn, caller, permission, resource).await;
        }
        return Err(error.into());
    }
    Ok(())
}

pub(super) async fn record_denial(
    conn: &mut PgConnection,
    caller: &bergsmith_persistence::auth::Session,
    permission: Permission,
    resource: ResourceScope,
) {
    let (resource_type, id) = match resource {
        ResourceScope::Organization => ("organization", caller.organization_id.to_string()),
        ResourceScope::Connection(id) => ("catalog", id.to_string()),
        ResourceScope::Table(id) => ("table", id.to_string()),
    };
    if let Err(error) = bergsmith_persistence::audit::record(
        conn,
        &bergsmith_persistence::audit::NewAuditEvent {
            organization_id: Some(caller.organization_id),
            actor_principal_id: Some(caller.principal_id),
            actor_kind: bergsmith_common::audit::ActorKind::User,
            action: bergsmith_common::audit::AuditAction::ApiPermissionDenied.as_str(),
            resource_type,
            resource_id: Some(&id),
            outcome: bergsmith_common::audit::AuditOutcome::Denied,
            detail: json!({"permission": permission.as_str()}),
        },
    )
    .await
    {
        tracing::warn!(error = %error, "api permission-denial audit record failed");
    }
}

pub(super) async fn audit(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<PageQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let page = Page::parse(
        query.map_err(|_| ApiError::invalid())?.0,
        caller.organization_id,
        caller.principal_id,
        "audit",
    )?;
    let after = page
        .after
        .as_deref()
        .map(str::parse::<i64>)
        .transpose()
        .map_err(|_| ApiError::invalid())?;
    if after.is_some_and(|id| id < 0) {
        return Err(ApiError::invalid());
    }
    let mut conn = state.database.pool().acquire().await?;
    require(
        &mut conn,
        &caller.0,
        Permission::ViewAudit,
        ResourceScope::Organization,
    )
    .await?;
    let rows = bergsmith_persistence::audit::list(
        &mut conn,
        caller.organization_id,
        after,
        i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?,
    )
    .await?;
    let rows = rows.into_iter().map(|row| (row.id.to_string(), json!({
        // Encode bigint IDs as decimal strings so JavaScript preserves precision.
        "id": row.id.to_string(), "actor_principal_id": row.actor_principal_id,
        "actor_kind": row.actor_kind.as_str(), "action": row.action,
        "resource_type": row.resource_type, "resource_id": row.resource_id,
        "outcome": row.outcome.as_str(), "detail": row.detail, "occurred_at": row.occurred_at,
    }))).collect();
    Ok(Json(page.finish(rows)?))
}

pub(super) async fn deferred_preview(
    State(state): State<ApiState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    let mut conn = state.database.pool().acquire().await?;
    // No table or preview is read until T2 supplies a typed request and scope.
    let grants =
        grants::active_grants(&mut conn, caller.organization_id, caller.principal_id).await?;
    if !grants
        .iter()
        .any(|grant| grant.role.allows(Permission::SubmitJobs))
    {
        record_denial(
            &mut conn,
            &caller.0,
            Permission::SubmitJobs,
            ResourceScope::Organization,
        )
        .await;
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "request_permission",
        ));
    }
    Err(ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "previews_unavailable",
        "wait_for_preview_api",
    ))
}

pub(super) async fn deferred_schedules(
    State(state): State<ApiState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    let mut conn = state.database.pool().acquire().await?;
    require(
        &mut conn,
        &caller.0,
        Permission::ManageSchedules,
        ResourceScope::Organization,
    )
    .await?;
    Err(ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "scheduling_unavailable",
        "wait_for_scheduling_api",
    ))
}
