//! Audited catalog configuration commands (BS-022.T1).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bergsmith_common::audit::{ActorKind, AuditAction, AuditOutcome};
use bergsmith_common::ids::CatalogConnectionId;
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::audit::NewAuditEvent;
use bergsmith_persistence::tenancy::{self, CatalogConnection, NewCatalogConnection};
use bergsmith_persistence::{PersistenceError, grants};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{ApiError, ApiState, Caller, json_body, resources};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateCatalog {
    name: String,
    uri: String,
    warehouse: Option<String>,
    credential_ref: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UpdateCatalog {
    expected_revision: i64,
    name: Option<String>,
    uri: Option<String>,
    #[serde(default, deserialize_with = "super::scheduling::double_option")]
    #[expect(
        clippy::option_option,
        reason = "absent preserves a field and null clears it"
    )]
    warehouse: Option<Option<String>>,
    #[serde(default, deserialize_with = "super::scheduling::double_option")]
    #[expect(
        clippy::option_option,
        reason = "absent preserves a field and null clears it"
    )]
    credential_ref: Option<Option<String>>,
    enabled: Option<bool>,
}

pub(super) async fn create(
    State(state): State<ApiState>,
    caller: Caller,
    body: Result<Json<CreateCatalog>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let body = json_body(body)?;
    let mut tx = authorized_transaction(&state, &caller, ResourceScope::Organization).await?;
    validate_fields(
        Some(&body.name),
        Some(&body.uri),
        body.warehouse.as_deref(),
        body.credential_ref.as_deref(),
    )?;
    let new = NewCatalogConnection {
        id: CatalogConnectionId::generate(),
        organization_id: caller.organization_id,
        name: body.name,
        uri: body.uri,
        warehouse: body.warehouse,
        credential_ref: body.credential_ref,
    };
    let created = tenancy::create_catalog_connection(&mut tx, &new).await?;
    record(&mut tx, &caller, &created, AuditAction::CatalogCreated).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, format!("/api/v1/catalogs/{}", created.id))],
        Json(document(&created)),
    )
        .into_response())
}

pub(super) async fn update(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
    body: Result<Json<UpdateCatalog>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = CatalogConnectionId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    let body = json_body(body)?;
    if body.expected_revision < 0 {
        return Err(ApiError::invalid());
    }
    let mut tx = authorized_transaction(&state, &caller, ResourceScope::Connection(id)).await?;
    if body.name.is_none()
        && body.uri.is_none()
        && body.warehouse.is_none()
        && body.credential_ref.is_none()
        && body.enabled.is_none()
    {
        return Err(ApiError::invalid());
    }
    validate_fields(
        body.name.as_deref(),
        body.uri.as_deref(),
        body.warehouse.as_ref().and_then(|value| value.as_deref()),
        body.credential_ref
            .as_ref()
            .and_then(|value| value.as_deref()),
    )?;
    let current = tenancy::catalog_connection(&mut tx, caller.organization_id, id).await?;
    let new = NewCatalogConnection {
        id,
        organization_id: caller.organization_id,
        name: body.name.unwrap_or(current.name),
        uri: body.uri.unwrap_or(current.uri),
        warehouse: body.warehouse.unwrap_or(current.warehouse),
        credential_ref: body.credential_ref.unwrap_or(current.credential_ref),
    };
    let updated = tenancy::update_catalog_connection(
        &mut tx,
        &new,
        body.expected_revision,
        body.enabled.unwrap_or(current.enabled),
    )
    .await?;
    record(&mut tx, &caller, &updated, AuditAction::CatalogUpdated).await?;
    tx.commit().await?;
    Ok(Json(document(&updated)))
}

/// Authorizes once inside the mutation transaction. On denial, roll back first
/// so the best-effort denial audit survives outside that transaction.
async fn authorized_transaction(
    state: &ApiState,
    caller: &Caller,
    scope: ResourceScope,
) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, ApiError> {
    let mut tx = state.database.pool().begin().await?;
    if let Err(error) = grants::require_permission(
        &mut tx,
        caller.organization_id,
        caller.principal_id,
        Permission::ManageConnections,
        scope,
    )
    .await
    {
        // Cleanup and best-effort auditing must not replace the authorization
        // error with a rollback or pool-acquisition failure.
        if tx.rollback().await.is_err() {
            tracing::warn!("catalog authorization rollback failed");
        }
        if matches!(error, PersistenceError::PermissionDenied { .. })
            && let Ok(mut conn) = state.database.pool().acquire().await
        {
            resources::record_denial(&mut conn, &caller.0, Permission::ManageConnections, scope)
                .await;
        }
        return Err(error.into());
    }
    Ok(tx)
}

/// Validates supplied fields, preserving legacy values when a PATCH omits them.
fn validate_fields(
    name: Option<&str>,
    uri: Option<&str>,
    warehouse: Option<&str>,
    credential_ref: Option<&str>,
) -> Result<(), ApiError> {
    if let Some(name) = name {
        let name = name.as_bytes();
        if name.is_empty()
            || name.len() > 63
            || !name[0].is_ascii_lowercase() && !name[0].is_ascii_digit()
            || !name
                .iter()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-'))
        {
            return Err(ApiError::invalid());
        }
    }
    if let Some(raw) = uri {
        let uri = url::Url::parse(raw).map_err(|_| ApiError::invalid())?;
        if !matches!(uri.scheme(), "http" | "https")
            || uri.host_str().is_none()
            || !uri.username().is_empty()
            || uri.password().is_some()
            || uri.query().is_some()
            || uri.fragment().is_some()
            || raw.len() > 2048
            || raw.chars().any(char::is_whitespace)
        {
            return Err(ApiError::invalid());
        }
    }
    if warehouse.is_some_and(|value| {
        value.trim().is_empty() || value.len() > 2048 || value.chars().any(char::is_control)
    }) || credential_ref.is_some_and(|value| {
        value.is_empty()
            || value.len() > 200
            || !value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'/' | b'_' | b'-' | b'.'))
    }) {
        return Err(ApiError::invalid());
    }
    Ok(())
}

/// Configuration is returned only to callers who may manage this connection.
pub(super) fn document(connection: &CatalogConnection) -> Value {
    let mut document = json!({"id":connection.id, "name":connection.name,
        "warehouse":connection.warehouse, "credential_ref":connection.credential_ref,
        "enabled":connection.enabled, "revision":connection.revision,
        "created_at":connection.created_at, "updated_at":connection.updated_at});
    // Legacy operator-created rows only had a scheme check. Never expose
    // embedded authentication from those rows through the new detail API.
    if let Ok(mut uri) = url::Url::parse(&connection.uri) {
        let _ = uri.set_username("");
        let _ = uri.set_password(None);
        uri.set_query(None);
        uri.set_fragment(None);
        document["uri"] = json!(uri.as_str());
    }
    document
}

async fn record(
    conn: &mut sqlx::PgConnection,
    caller: &Caller,
    connection: &CatalogConnection,
    action: AuditAction,
) -> Result<(), ApiError> {
    bergsmith_persistence::audit::record(
        conn,
        &NewAuditEvent {
            organization_id: Some(caller.organization_id),
            actor_principal_id: Some(caller.principal_id),
            actor_kind: ActorKind::User,
            action: action.as_str(),
            resource_type: "catalog",
            resource_id: Some(&connection.id.to_string()),
            outcome: AuditOutcome::Allowed,
            // Do not copy URIs, warehouse values, or credential references into audit.
            detail: json!({"revision":connection.revision,"enabled":connection.enabled}),
        },
    )
    .await?;
    Ok(())
}
