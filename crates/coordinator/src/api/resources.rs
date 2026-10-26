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

// The aliases of table `$table` the caller may see: an organization or table
// grant reveals every alias, a connection grant only that connection's.
macro_rules! visible_aliases {
    ($table:literal) => {
        concat!(
            "COALESCE((SELECT jsonb_agg(jsonb_build_object(
              'catalog_id', a.catalog_connection_id, 'namespace', a.namespace,
              'name', a.table_name, 'revision', a.revision)
              ORDER BY a.catalog_connection_id, a.namespace, a.table_name)
            FROM table_aliases a WHERE a.organization_id = $1 AND a.table_identity_id = ",
            $table,
            "
              AND EXISTS (SELECT 1 FROM principal_role_grants g
                WHERE g.organization_id = $1 AND g.principal_id = $2
                  AND g.revoked_at IS NULL AND g.role = ANY($3::text[])
                  AND ((g.catalog_connection_id IS NULL AND g.table_identity_id IS NULL)
                    OR g.table_identity_id = ",
            $table,
            "
                    OR g.catalog_connection_id = a.catalog_connection_id))), '[]'::jsonb)"
        )
    };
}

macro_rules! table_summary {
    () => {
        concat!(
            "jsonb_build_object('id', r.id, 'table_uuid', r.table_uuid, 'created_at', r.created_at,
          'aliases', ",
            visible_aliases!("r.id"),
            ", 'statistics', (SELECT jsonb_build_object(
                'sampled_at', s.sampled_at,
                'record_count', s.record_count::text,
                'data_files', s.data_files::text,
                'data_bytes', s.data_bytes::text,
                'delete_files', s.delete_files::text,
                'small_files', s.small_files::text)
              FROM table_statistics s
              WHERE s.organization_id = r.organization_id AND s.table_identity_id = r.id
              ORDER BY s.sampled_at DESC LIMIT 1))"
        )
    };
}

// Names are for display; `table_id` and `created_by` stay the keys. The
// requester's foreign key names a principal of the job's organization, and
// display names are required, so `created_by_name` is never null.
macro_rules! job_summary {
    () => {
        concat!(
            "jsonb_build_object('id', r.id, 'table_id', r.table_identity_id, 'table_aliases', ",
            visible_aliases!("r.table_identity_id"),
            ", 'state', r.state, 'revision', r.state_revision, 'state_reason', r.state_reason, \
             'retry_of_job_id', r.retry_of_job_id, 'created_by', r.created_by, \
             'created_by_name', (SELECT p.display_name FROM principals p \
                WHERE p.id = r.created_by AND p.organization_id = r.organization_id), \
             'created_at', r.created_at, 'updated_at', r.updated_at, \
             'cancellation_requested', r.cancellation_requested_at IS NOT NULL, \
             'publication_started', r.publication_started_at IS NOT NULL, \
             'trigger', CASE WHEN r.trigger_finding IS NULL THEN NULL \
                ELSE jsonb_build_object('kind', r.trigger_finding, \
                                        'severity', r.trigger_severity) END)"
        )
    };
}

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
    const fn name(self) -> &'static str {
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
                "jsonb_build_object('id', r.id, 'name', r.name, 'enabled', r.enabled, 'settings', jsonb_build_object('maintenance_enabled', r.maintenance_enabled, 'max_execution_seconds', r.max_execution_seconds, 'max_unresolved_publication_seconds', r.max_unresolved_publication_seconds), 'revision', r.revision, 'updated_at', r.updated_at, 'created_at', r.created_at)",
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
        "actor_display_name": row.actor_display_name, "actor_kind": row.actor_kind.as_str(),
        "action": row.action,
        "resource_type": row.resource_type, "resource_id": row.resource_id,
        "outcome": row.outcome.as_str(), "detail": row.detail, "occurred_at": row.occurred_at,
    }))).collect();
    Ok(Json(page.finish(rows)?))
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
