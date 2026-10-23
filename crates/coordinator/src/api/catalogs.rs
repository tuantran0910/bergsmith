//! Audited catalog configuration commands (BS-022.T1).
//!
//! The commands change how a connection is configured. The capability check
//! (BS-022.T2) only reads what a catalog advertises and records the answer.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bergsmith_catalog::{CapabilityReport, ConnectionConfig};
use bergsmith_common::audit::{ActorKind, AuditAction, AuditOutcome};
use bergsmith_common::capabilities::{
    CapabilityState, ConnectionCapabilities, routing_fingerprint,
};
use bergsmith_common::ids::CatalogConnectionId;
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::audit::NewAuditEvent;
use bergsmith_persistence::capabilities::CapabilityCheck;
use bergsmith_persistence::tenancy::{self, CatalogConnection, NewCatalogConnection};
use bergsmith_persistence::{PersistenceError, grants};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;
use uuid::Uuid;

use crate::checks::{CapabilityCheckLimits, CheckError};

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
        Json(document(&created, None)),
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
    Ok(Json(document(&updated, None)))
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
        let uri = Url::parse(raw).map_err(|_| ApiError::invalid())?;
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
pub(super) fn document(connection: &CatalogConnection, check: Option<&CapabilityCheck>) -> Value {
    let mut document = json!({"id":connection.id, "name":connection.name,
        "warehouse":connection.warehouse, "credential_ref":connection.credential_ref,
        "enabled":connection.enabled, "revision":connection.revision,
        "created_at":connection.created_at, "updated_at":connection.updated_at});
    // Legacy operator-created rows only had a scheme check. Never expose
    // embedded authentication from those rows through the new detail API.
    if let Ok(mut uri) = Url::parse(&connection.uri) {
        let _ = uri.set_username("");
        let _ = uri.set_password(None);
        uri.set_query(None);
        uri.set_fragment(None);
        document["uri"] = json!(uri.as_str());
    }
    // The last capability check is diagnostic, not configuration, but only a
    // caller who may manage this connection sees the answer.
    if let Some(check) = check {
        document["capabilities"] = capabilities_json(&check.capabilities);
        document["capabilities_checked_at"] = json!(check.checked_at);
    }
    document
}

/// The capability record as the API returns it.
fn capabilities_json(capabilities: &ConnectionCapabilities) -> Value {
    json!({
        "list_tables": capabilities.list_tables.as_str(),
        "load_table": capabilities.load_table.as_str(),
        "commit_table": capabilities.commit_table.as_str(),
        "vended_credentials": capabilities.vended_credentials.as_str(),
        "credentials_endpoint": capabilities.credentials_endpoint.as_str(),
        // A decimal string: a lifetime may be larger than a browser counts
        // exactly, and the advertised value is never changed.
        "idempotency_key_lifetime_seconds": capabilities
            .idempotency_key_lifetime_seconds
            .map(|seconds| seconds.to_string()),
    })
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

/// Checks what a connection's catalog advertises and records the answer.
///
/// The check reads `GET /v1/config` and nothing else. It never changes the
/// connection's configuration or its revision: it is a diagnostic an operator
/// runs before trusting a connection, so the ledger is released before the
/// catalog is contacted and the answer is written afterwards.
///
/// The answer is bound to the settings it describes. If the connection's
/// routing changed while the catalog was answering, the answer describes a
/// configuration that no longer exists, so it is not recorded and the caller
/// is told to run the check again.
///
/// # Errors
///
/// Returns 403 without `connections:manage` for the connection, 404 for an
/// unknown connection, 409 when the connection changed during the check, a
/// permanent error when the connection cannot be used as it is stored, and a
/// transient error when the catalog cannot be reached or the coordinator is
/// at its check capacity.
pub(super) async fn check(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = CatalogConnectionId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    let mut tx = authorized_transaction(&state, &caller, ResourceScope::Connection(id)).await?;
    let connection = tenancy::catalog_connection(&mut tx, caller.organization_id, id).await?;
    // The catalog call is network I/O, so the ledger connection is released
    // before it starts.
    tx.commit().await?;
    let config = catalog_config(&connection, state.checks.limits())?;
    let probed = fingerprint(&connection);
    let report = state
        .checks
        .check(caller.principal_id, &config)
        .await
        .map_err(|error| classify_check(&error))?;
    let capabilities = capabilities(&report);
    let mut tx = state.database.pool().begin().await?;
    // The shared lock keeps an edit from landing between this comparison and
    // the write below.
    let current =
        tenancy::catalog_connection_for_share(&mut tx, caller.organization_id, id).await?;
    if fingerprint(&current) != probed {
        return Err(ApiError::conflict(
            "catalog_connection_changed",
            "run_check_again",
        ));
    }
    let checked_at = bergsmith_persistence::capabilities::record_check(
        &mut tx,
        caller.organization_id,
        id,
        &capabilities,
        &probed,
    )
    .await?;
    record_check(&mut tx, &caller, id, &capabilities, checked_at).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "catalog_id": id,
        "checked_at": checked_at,
        "capabilities": capabilities_json(&capabilities),
    })))
}

/// The fingerprint of the settings a check depends on.
pub(super) fn fingerprint(connection: &CatalogConnection) -> String {
    routing_fingerprint(
        &connection.uri,
        connection.warehouse.as_deref(),
        connection.credential_ref.as_deref(),
    )
}

/// The catalog configuration of a stored connection.
///
/// A connection that names an external credential cannot be checked: the
/// coordinator does not resolve credential references yet (ADR 0035), and it
/// never falls back to ambient identity.
fn catalog_config(
    connection: &CatalogConnection,
    limits: CapabilityCheckLimits,
) -> Result<ConnectionConfig, ApiError> {
    if connection.credential_ref.is_some() {
        return Err(ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_credential_unavailable",
            "configure_catalog_credential",
        ));
    }
    let uri = Url::parse(&connection.uri).map_err(|_| {
        ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_connection_invalid",
            "fix_catalog_connection",
        )
    })?;
    let mut config = ConnectionConfig::new(uri);
    config.warehouse.clone_from(&connection.warehouse);
    config.connect_timeout = limits.connect_timeout;
    config.request_timeout = limits.request_timeout;
    Ok(config)
}

/// Reads the catalog's configuration within a bounded time.
/// Classifies a check failure.
///
/// The catalog mapping matches the preview contract, so one catalog failure
/// reads the same way wherever it happens. A check that could not start
/// because the coordinator is at its bound is transient: the same request
/// succeeds once a running check finishes.
fn classify_check(error: &CheckError) -> ApiError {
    match error {
        CheckError::Overloaded => {
            ApiError::transient("capability_check_capacity_exhausted", "retry_same_request")
        }
        CheckError::Timeout => ApiError::transient("catalog_unavailable", "retry_same_request"),
        CheckError::Catalog(catalog) => classify(catalog),
    }
}

/// The capability record of one catalog's answer.
///
/// Vended credentials stay unknown: the configuration does not say whether a
/// catalog vends table-scoped credentials. Loading a table proves it.
fn capabilities(report: &CapabilityReport) -> ConnectionCapabilities {
    let state = |supported: bool| {
        if supported {
            CapabilityState::Met
        } else {
            CapabilityState::Missing
        }
    };
    ConnectionCapabilities {
        list_tables: state(report.supports_list_tables()),
        load_table: state(report.supports_load_table()),
        commit_table: state(report.supports_commit_table()),
        vended_credentials: CapabilityState::Unknown,
        credentials_endpoint: state(report.credentials_refresh),
        idempotency_key_lifetime_seconds: report
            .idempotency_key_lifetime
            .map(|lifetime| lifetime.as_secs()),
    }
}

/// Records the check and its outcome in the audit log.
async fn record_check(
    conn: &mut sqlx::PgConnection,
    caller: &Caller,
    id: CatalogConnectionId,
    capabilities: &ConnectionCapabilities,
    checked_at: DateTime<Utc>,
) -> Result<(), ApiError> {
    let missing: Vec<&str> = [
        ("list_tables", capabilities.list_tables),
        ("load_table", capabilities.load_table),
        ("commit_table", capabilities.commit_table),
        ("credentials_endpoint", capabilities.credentials_endpoint),
    ]
    .into_iter()
    .filter(|(_, state)| !matches!(state, CapabilityState::Met))
    .map(|(requirement, _)| requirement)
    .collect();
    bergsmith_persistence::audit::record(
        conn,
        &NewAuditEvent {
            organization_id: Some(caller.organization_id),
            actor_principal_id: Some(caller.principal_id),
            actor_kind: ActorKind::User,
            action: AuditAction::CatalogChecked.as_str(),
            resource_type: "catalog",
            resource_id: Some(&id.to_string()),
            outcome: AuditOutcome::Allowed,
            // The answer names requirements, never catalog content.
            detail: json!({
                "checked_at": checked_at,
                "complete": capabilities.complete(),
                "missing": missing,
            }),
        },
    )
    .await?;
    Ok(())
}

/// Classifies a catalog failure without logging it.
///
/// The mapping matches the preview contract, so one catalog failure reads the
/// same way wherever it happens.
fn classify(error: &bergsmith_catalog::CatalogError) -> ApiError {
    use bergsmith_catalog::CatalogError;
    match error {
        CatalogError::Status {
            status: 401 | 403 | 419,
            ..
        } => ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_access_denied",
            "check_catalog_credentials",
        ),
        CatalogError::Status {
            status: 408 | 425 | 429,
            ..
        }
        | CatalogError::Authentication {
            status: 408 | 425 | 429,
        } => ApiError::transient("catalog_unavailable", "retry_same_request"),
        CatalogError::Status { status, .. } if *status >= 500 => {
            ApiError::transient("catalog_unavailable", "retry_same_request")
        }
        CatalogError::Status { .. } => ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_request_rejected",
            "check_catalog_compatibility",
        ),
        CatalogError::Authentication { status } if *status >= 500 => {
            ApiError::transient("catalog_unavailable", "retry_same_request")
        }
        CatalogError::Authentication { .. } => ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_access_denied",
            "check_catalog_credentials",
        ),
        CatalogError::MissingEndpoints(_) | CatalogError::IdempotencyUnsupported => {
            ApiError::permanent(
                StatusCode::INTERNAL_SERVER_ERROR,
                "catalog_unsupported",
                "check_catalog_compatibility",
            )
        }
        CatalogError::Credentials(_) | CatalogError::NoVendedCredentials => ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_credentials_unavailable",
            "configure_catalog_credential_vending",
        ),
        CatalogError::InvalidUrl(_) => ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_connection_invalid",
            "fix_catalog_connection",
        ),
        CatalogError::InvalidLocation(_) | CatalogError::DataLocationOutsideTable(_) => {
            ApiError::permanent(
                StatusCode::CONFLICT,
                "table_metadata_inconsistent",
                "fix_table_location",
            )
        }
        CatalogError::Transport { .. } | CatalogError::Decode { .. } => {
            ApiError::transient("catalog_unavailable", "retry_same_request")
        }
        CatalogError::ResponseTooLarge { .. } => ApiError::permanent(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_response_too_large",
            "check_catalog_configuration",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_and_a_given_up_check_are_transient() {
        let refused = classify_check(&CheckError::Overloaded);
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.reason, "capability_check_capacity_exhausted");

        let given_up = classify_check(&CheckError::Timeout);
        assert_eq!(given_up.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(given_up.reason, "catalog_unavailable");
    }

    #[test]
    fn an_oversized_answer_is_permanent() {
        let error = classify(&bergsmith_catalog::CatalogError::ResponseTooLarge {
            operation: "GET /v1/config",
            max_bytes: 1024,
        });
        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.reason, "catalog_response_too_large");
    }
}
