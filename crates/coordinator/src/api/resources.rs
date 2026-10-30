use axum::Json;
use axum::extract::{Path, Query, State};
use bergsmith_common::job::JobState;
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::grants;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::pagination::{Page, PageQuery};
use super::{ApiError, ApiState, Caller};
use crate::identity::administration::AdministrationError;
use crate::identity::authorization::AuthorizationError;

// Queries are compile-time literals (see `sql`). All request values are bound.

// Logical input sizes come from the immutable plan, and only tasks with an
// accepted receipt count as rewritten. Output objects are verified at receipt
// acceptance. Failed/retried attempts never inflate the result. This projection
// is detail-only: inventory lists and SSE permission checks remain inexpensive.
macro_rules! job_statistics {
    () => {
        "(WITH accepted AS (
            SELECT rc.task_id, rc.attempt_id FROM receipts rc
            JOIN tasks task ON task.id = rc.task_id WHERE task.job_id = r.id
          ), rewritten AS (
            SELECT task FROM plans p
            CROSS JOIN LATERAL jsonb_array_elements(COALESCE(p.content->'tasks', '[]'::jsonb)) task
            JOIN accepted a ON a.task_id::text = task->>'task_id'
            WHERE p.job_id = r.id
          ), sources AS (
            SELECT jsonb_array_elements(COALESCE(task->'sources', '[]'::jsonb)) source FROM rewritten
          ), deletes AS (
            SELECT jsonb_array_elements(COALESCE(task->'deletes', '[]'::jsonb)) entry FROM rewritten
          ), outputs AS (
            SELECT o.size_bytes FROM attempt_output_objects o
            JOIN accepted a ON a.attempt_id = o.attempt_id
          ) SELECT jsonb_build_object(
            'plan_recorded', EXISTS(SELECT 1 FROM plans WHERE job_id = r.id),
            'planned_input_files', (SELECT content #>> '{estimate,files}' FROM plans WHERE job_id = r.id),
            'planned_input_bytes', (SELECT content #>> '{estimate,bytes}' FROM plans WHERE job_id = r.id),
            'tasks_total', (SELECT count(*)::text FROM tasks WHERE job_id = r.id),
            'tasks_accepted', (SELECT count(*)::text FROM accepted),
            'attempts', (SELECT count(*)::text FROM attempts a JOIN tasks t ON t.id = a.task_id WHERE t.job_id = r.id),
            'input_files', (SELECT count(*)::text FROM sources),
            'input_bytes', (SELECT COALESCE(sum((source->>'file_size_bytes')::numeric), 0)::text FROM sources),
            'output_files', (SELECT count(*)::text FROM outputs),
            'output_bytes', (SELECT COALESCE(sum(size_bytes::numeric), 0)::text FROM outputs),
            'delete_files_applied', (SELECT count(DISTINCT entry->>'path')::text FROM deletes),
            'elapsed_millis', FLOOR(GREATEST(0, EXTRACT(EPOCH FROM (
              CASE WHEN r.state IN ('Succeeded','NoOp','Cancelled','ReplanRequired','Failed')
                THEN r.updated_at ELSE statement_timestamp() END - r.created_at)) * 1000))::text,
            'publication_confirmed', r.state = 'Succeeded'))"
    };
}

#[derive(Clone, Copy)]
enum Selection {
    Item,
    First,
    Continuation,
}

#[derive(Clone, Copy)]
pub(super) enum Resource {
    Catalog,
    Table,
    Job,
    JobDetail,
}

impl Resource {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Catalog => "catalogs",
            Self::Table => "tables",
            Self::Job | Self::JobDetail => "jobs",
        }
    }
    const fn permission(self) -> Permission {
        match self {
            Self::Catalog | Self::Table => Permission::ViewInventory,
            Self::Job | Self::JobDetail => Permission::ViewJobs,
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
                catalog_summary!(),
                catalog_access!()
            ),
            Self::Table => variants!(
                "table_identities r JOIN table_identities t ON t.id = r.id",
                table_summary!(),
                table_access!()
            ),
            Self::Job => variants!(
                "jobs r JOIN table_identities t ON t.id = r.table_identity_id AND t.organization_id = r.organization_id",
                job_summary!(),
                table_access!()
            ),
            Self::JobDetail => variants!(
                "jobs r JOIN table_identities t ON t.id = r.table_identity_id AND t.organization_id = r.organization_id",
                concat!(
                    job_summary!(),
                    " || jsonb_build_object('statistics', ",
                    job_statistics!(),
                    ")"
                ),
                table_access!()
            ),
        }
    }
}

pub(super) async fn read(
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
    let rows = read_page(state, caller, resource, &page, id).await?;
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

/// One page of `resource` rows within `page`, or the caller's `id` of it.
pub(super) async fn read_page(
    state: &ApiState,
    caller: &bergsmith_persistence::auth::Session,
    resource: Resource,
    page: &Page,
    id: Option<Uuid>,
) -> Result<Vec<(Uuid, Value)>, ApiError> {
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
    Ok(rows)
}

/// The summaries of `ids` the caller may see, in ascending id order.
///
/// A table that stops being visible between the scan and this read is simply
/// absent, which keeps a page's grant filtering identical to the plain list.
pub(super) async fn table_summaries(
    state: &ApiState,
    caller: &bergsmith_persistence::auth::Session,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, Value)>, ApiError> {
    let roles: Vec<_> = Resource::Table
        .permission()
        .roles()
        .iter()
        .map(|role| role.as_str())
        .collect();
    let rows: Vec<(Uuid, Value)> = sqlx::query_as(concat!(
        "SELECT r.id, ",
        table_summary!(),
        " AS document FROM table_identities r JOIN table_identities t ON t.id = r.id \
         WHERE r.organization_id = $1 AND ",
        table_access!(),
        " AND r.id = ANY($4) ORDER BY r.id"
    ))
    .bind(caller.organization_id.as_uuid())
    .bind(caller.principal_id.as_uuid())
    .bind(roles)
    .bind(ids.to_vec())
    .fetch_all(state.database.pool())
    .await?;
    Ok(rows)
}

pub(super) async fn catalog(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = id.map_err(|_| ApiError::invalid())?.0;
    let summary = read(
        &state,
        &caller.0,
        Resource::Catalog,
        PageQuery::default(),
        Some(id),
    )
    .await?;
    let mut tx = state.database.pool().begin().await?;
    let connection_id = bergsmith_common::ids::CatalogConnectionId::from_uuid(id);
    if grants::has_permission(
        &mut tx,
        caller.organization_id,
        caller.principal_id,
        Permission::ManageConnections,
        ResourceScope::Connection(connection_id),
    )
    .await?
    {
        // The shared lock keeps an edit from landing between reading the
        // connection and the answer recorded for it.
        let connection = bergsmith_persistence::tenancy::catalog_connection_for_share(
            &mut tx,
            caller.organization_id,
            connection_id,
        )
        .await?;
        let check = bergsmith_persistence::capabilities::latest_check(
            &mut tx,
            caller.organization_id,
            connection_id,
        )
        .await?;
        // An answer recorded under different routing describes a connection
        // that no longer exists, so it is not shown.
        let check = check
            .filter(|check| check.config_fingerprint == super::catalogs::fingerprint(&connection));
        // The effective profile and the unresolved publications are read-only
        // reports, so they are added to the configuration, never stored with it.
        let document =
            super::catalogs::document_with_reports(&state, &mut tx, &connection, check.as_ref())
                .await?;
        tx.commit().await?;
        return Ok(Json(document));
    }
    tx.commit().await?;
    Ok(summary)
}
pub(super) async fn job(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    read(
        &state,
        &caller.0,
        Resource::JobDetail,
        PageQuery::default(),
        Some(id.map_err(|_| ApiError::invalid())?.0),
    )
    .await
}

/// The state of a job the caller may view, using the same predicate as
/// `GET /api/v1/jobs/{id}`: an unknown or inaccessible job is 404.
pub(super) async fn visible_job_state(
    state: &ApiState,
    caller: &bergsmith_persistence::auth::Session,
    id: Uuid,
) -> Result<JobState, ApiError> {
    let Json(job) = read(state, caller, Resource::Job, PageQuery::default(), Some(id)).await?;
    job["state"]
        .as_str()
        .and_then(JobState::from_name)
        .ok_or_else(|| {
            tracing::error!(job_id = %id, "job resource has no valid state");
            ApiError::internal()
        })
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

/// Maps an administration failure, recording a denial that the transactional
/// check found after the request had already passed the route pre-check.
///
/// The administration transaction has rolled back by the time this runs, so
/// the denial is written on the handler's own connection. That keeps it out of
/// a second pool acquisition, which a small pool could refuse.
pub(super) async fn administered<T>(
    conn: &mut PgConnection,
    caller: &Caller,
    permission: Permission,
    resource: ResourceScope,
    result: Result<T, AdministrationError>,
) -> Result<T, ApiError> {
    match result {
        Ok(value) => Ok(value),
        Err(error @ AdministrationError::Authorization(AuthorizationError::Denied { .. })) => {
            record_denial(conn, &caller.0, permission, resource).await;
            Err(error.into())
        }
        Err(error) => Err(error.into()),
    }
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

/// `GET /api/v1/workers`: the organization's current worker sessions.
///
/// Requires organization-wide `workers:manage`. Sessions never expose tokens
/// or credentials; byte counts fit a JSON number.
pub(super) async fn workers(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<PageQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let page = Page::parse(
        query.map_err(|_| ApiError::invalid())?.0,
        caller.organization_id,
        caller.principal_id,
        "workers",
    )?;
    let after = page
        .after
        .as_deref()
        .map(str::parse::<Uuid>)
        .transpose()
        .map_err(|_| ApiError::invalid())?
        .map(bergsmith_common::ids::WorkerSessionId::from_uuid);
    let mut conn = state.database.pool().acquire().await?;
    require(
        &mut conn,
        &caller.0,
        Permission::ManageWorkers,
        ResourceScope::Organization,
    )
    .await?;
    let sessions = bergsmith_persistence::workers::list_sessions(
        &mut conn,
        caller.organization_id,
        after,
        i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?,
    )
    .await?;
    let rows = sessions
        .into_iter()
        .map(|session| {
            (
                session.id.to_string(),
                json!({
                    "id": session.id, "principal_id": session.principal_id,
                    "pool": session.pool, "worker_name": session.worker_name,
                    "build_version": session.build_version,
                    "capabilities": session.capabilities,
                    "memory_bytes": session.memory_bytes, "cpu_millis": session.cpu_millis,
                    "status": session.status.as_str(),
                    "registered_at": session.registered_at,
                    "lease_expires_at": session.lease_expires_at,
                    "running_attempt_id": session.running_attempt_id,
                }),
            )
        })
        .collect();
    Ok(Json(page.finish(rows)?))
}
