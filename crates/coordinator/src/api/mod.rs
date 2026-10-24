//! Versioned browser API (BS-018.T1–T3).
//!
//! Merge [`router`] with the identity router. Every resource request uses the
//! server-side session; mutations additionally require the session's CSRF token.
//! Organization and actor identifiers always come from that session.

mod catalogs;
mod commands;
mod creation;
mod discovery;
mod events;
mod pagination;
mod preview;
mod resources;
mod scheduling;
mod submission;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bergsmith_common::disposition::FailureDisposition;
use bergsmith_common::ids::{JobId, OrganizationId, PrincipalId};
use bergsmith_persistence::auth::Session;
use bergsmith_persistence::{Database, PersistenceError};
use serde::Serialize;
use serde_json::json;
use sqlx::PgConnection;

use crate::checks::CapabilityChecker;
use crate::discovery::CatalogDiscovery;
use crate::identity::{CSRF_HEADER, audit, routes::cookie, session};
use crate::preview::CatalogPreviewer;

pub use events::{EventStreamLimits, EventStreamLimitsError, JOB_EVENT_CHANNEL, JobEventHub};

/// Shared API dependencies. Uses the same session policy as `/auth`.
#[derive(Clone, Debug)]
pub struct ApiState {
    /// Durable resource and identity ledger.
    pub database: Database,
    /// Must match the configuration used to issue browser sessions.
    pub sessions: session::SessionConfig,
    /// Produces previews without creating work. The runtime supplies the
    /// storage profiles and planner configuration.
    pub previews: CatalogPreviewer,
    /// Admits and wakes job event streams. Start its
    /// [`listen`](JobEventHub::listen) task for prompt delivery; without it,
    /// streams poll.
    pub events: JobEventHub,
    /// Runs catalog capability checks within the process and requester bounds.
    pub checks: CapabilityChecker,
    /// Browses catalogs and loads the tables an operator registers.
    pub discovery: CatalogDiscovery,
}

/// Builds the `/api/v1` resource router, including its `OpenAPI` document.
///
/// Authentication runs before extraction, including malformed request bodies.
/// Responses are never cached. No catalog credentials or plan artifacts are
/// returned through these resource summaries.
///
/// The protected routes are assembled before the authentication layer so a new
/// resource route cannot become public by being registered later. Only the
/// contract document is added afterwards, deliberately unauthenticated.
pub fn router(state: ApiState) -> Router {
    let document = openapi_document(&state.sessions.cookie_name);
    if document.is_none() {
        tracing::error!("embedded OpenAPI document could not be prepared");
    }
    let protected = Router::new()
        .route(
            "/api/v1/catalogs",
            get(resources::catalogs).post(catalogs::create),
        )
        .route(
            "/api/v1/catalogs/{id}",
            get(resources::catalog).patch(catalogs::update),
        )
        .route("/api/v1/catalogs/{id}/check", post(catalogs::check))
        .route(
            "/api/v1/catalogs/{id}/namespaces",
            get(discovery::namespaces),
        )
        .route("/api/v1/catalogs/{id}/tables", get(discovery::tables))
        .route(
            "/api/v1/catalogs/{id}/registrations",
            post(discovery::register).delete(discovery::unregister),
        )
        .route("/api/v1/tables", get(resources::tables))
        .route("/api/v1/tables/{id}", get(resources::table))
        .route(
            "/api/v1/jobs",
            get(resources::jobs).post(submission::submit),
        )
        .route("/api/v1/jobs/{id}", get(resources::job))
        .route("/api/v1/jobs/{id}/cancel", post(commands::cancel))
        .route("/api/v1/jobs/{id}/retry", post(commands::retry))
        .route("/api/v1/jobs/{id}/events", get(events::events))
        .route("/api/v1/audit", get(resources::audit))
        .route("/api/v1/workers", get(resources::workers))
        .route("/api/v1/previews", post(preview::preview))
        .route(
            "/api/v1/policies",
            get(scheduling::policies).post(scheduling::create_policy),
        )
        .route(
            "/api/v1/policies/{id}",
            get(scheduling::policy).patch(scheduling::update_policy),
        )
        .route(
            "/api/v1/schedules",
            get(scheduling::schedules).post(scheduling::create_schedule),
        )
        .route(
            "/api/v1/schedules/{id}",
            get(scheduling::schedule).patch(scheduling::update_schedule),
        )
        .layer(DefaultBodyLimit::max(16 * 1024))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .merge(protected)
        // Public contract. It contains no deployment configuration.
        .route(
            "/api/v1/openapi.json",
            get(move || {
                let document = document.clone();
                async move { openapi(document) }
            }),
        )
        .fallback(|| async { ApiError::not_found() })
        .method_not_allowed_fallback(|| async {
            ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "check_api_contract",
            )
        })
        .layer(middleware::from_fn(no_store))
        .with_state(state)
}

/// Serves the document prepared once when the router was built.
fn openapi(document: Option<Bytes>) -> Result<Response, ApiError> {
    let Some(document) = document else {
        return Err(ApiError::unavailable());
    };
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(document))
        .map_err(|_| ApiError::unavailable())
}

/// Parses and serializes the checked-in contract once, with this deployment's
/// cookie name. Public requests never parse or allocate the document again.
fn openapi_document(cookie_name: &str) -> Option<Bytes> {
    let mut document: serde_json::Value =
        serde_json::from_str(include_str!("openapi.json")).ok()?;
    *document.pointer_mut("/components/securitySchemes/sessionCookie/name")? =
        serde_json::Value::String(cookie_name.to_owned());
    serde_json::to_vec(&document).ok().map(Bytes::from)
}

async fn no_store(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

async fn authenticate(
    State(state): State<ApiState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let raw = cookie(request.headers(), &state.sessions.cookie_name);
    let token = raw
        .as_deref()
        .and_then(|raw| session::SessionToken::parse(raw).ok());
    let Some(token) = token else {
        // An absent or malformed credential is unattributable. Persisting every
        // such denial would let unauthenticated clients grow the append-only
        // audit log and contend with real audit writers, so log it instead.
        tracing::debug!("api request rejected without a valid session token");
        return Err(ApiError::unauthorized());
    };
    let mut conn = state.database.pool().acquire().await?;
    let authenticated = match session::authenticate(&mut conn, &state.sessions, &token).await {
        Ok(session) => session,
        Err(session::SessionError::Persistence(error)) => {
            tracing::error!(error = %error, "api session lookup failed");
            return Err(ApiError::unavailable());
        }
        Err(error) => {
            tracing::debug!(error = %error, "api session rejected");
            let organization = match bergsmith_persistence::auth::session_organization(
                &mut conn,
                token.verifier(),
            )
            .await
            {
                Ok(Some(organization)) => organization,
                Ok(None) => {
                    // A correctly formatted token that was never issued has no
                    // organization to attribute. Persisting it would let anyone
                    // grow the append-only log without authenticating.
                    tracing::debug!("api rejected a well-formed but unknown session token");
                    return Err(ApiError::unauthorized());
                }
                Err(error) => {
                    tracing::warn!(error = %error, "api session attribution failed");
                    return Err(ApiError::unauthorized());
                }
            };
            // Best effort: the denial is already the safe outcome, and an audit
            // outage must not turn a client error into a retryable server error.
            audit_denial(&mut conn, Some(organization), None, "invalid_session").await;
            return Err(ApiError::unauthorized());
        }
    };
    if !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    ) {
        let csrf = request
            .headers()
            .get(CSRF_HEADER)
            .and_then(|value| value.to_str().ok());
        if csrf.is_none_or(|csrf| session::verify_csrf(&authenticated, csrf).is_err()) {
            audit_denial(
                &mut conn,
                Some(authenticated.organization_id),
                Some(authenticated.principal_id),
                "invalid_csrf",
            )
            .await;
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "csrf_rejected",
                "refresh_session",
            ));
        }
    }
    // Release the session-lookup connection before dispatch. Handlers acquire
    // only the connection they need, and a slow handler cannot pin this one.
    drop(conn);
    request.extensions_mut().insert(authenticated);
    Ok(next.run(request).await)
}

async fn audit_denial(
    conn: &mut PgConnection,
    organization: Option<OrganizationId>,
    actor: Option<PrincipalId>,
    reason: &str,
) {
    if let Err(error) = audit::session_denied(conn, organization, actor, reason).await {
        tracing::warn!(
            reason = error.reason_code(),
            "api session-denial audit record failed"
        );
    }
}

#[derive(Debug, Serialize)]
struct ApiError {
    #[serde(skip)]
    status: StatusCode,
    reason: &'static str,
    disposition: FailureDisposition,
    operator_action: &'static str,
}

impl ApiError {
    const fn new(status: StatusCode, reason: &'static str, operator_action: &'static str) -> Self {
        Self {
            status,
            reason,
            disposition: FailureDisposition::PermanentFailure,
            operator_action,
        }
    }
    /// A permanent failure with a specific status, such as a conflict.
    const fn permanent(
        status: StatusCode,
        reason: &'static str,
        operator_action: &'static str,
    ) -> Self {
        Self::new(status, reason, operator_action)
    }
    /// A transient failure: the same request can be retried unchanged.
    const fn transient(reason: &'static str, operator_action: &'static str) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            reason,
            disposition: FailureDisposition::RetrySameRequest,
            operator_action,
        }
    }
    const fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "authentication_required",
            "sign_in",
        )
    }
    const fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "resource_not_found",
            "check_resource_and_grants",
        )
    }
    const fn invalid() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "correct_request",
        )
    }
    const fn invalid_policy_parameters() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_policy_parameters",
            "correct_policy_parameters",
        )
    }
    const fn invalid_maintenance_window() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_maintenance_window",
            "use_a_daily_time_range",
        )
    }
    const fn conflict(reason: &'static str, operator_action: &'static str) -> Self {
        Self::permanent(StatusCode::CONFLICT, reason, operator_action)
    }
    const fn unavailable() -> Self {
        Self::transient("ledger_unavailable", "retry_same_request")
    }
    const fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "contact_operator",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self)).into_response()
    }
}

/// Whether a database failure is worth retrying unchanged.
///
/// Pool exhaustion and connection loss are transient. SQLSTATE classes 08
/// (connection), 40 (transaction rollback), 53 (resources), 57 (operator
/// intervention), and 58 (system) describe conditions that can clear.
fn is_transient_database_error(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_) => true,
        sqlx::Error::Database(database) => database.code().is_some_and(|code| {
            let code = code.as_ref();
            code.starts_with("08")
                || code.starts_with("40")
                || code.starts_with("53")
                || code.starts_with("57")
                || code.starts_with("58")
        }),
        _ => false,
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        if is_transient_database_error(&error) {
            tracing::warn!(error = %error, "api database operation failed");
            Self::unavailable()
        } else {
            tracing::error!(error = %error, "api database operation failed");
            Self::internal()
        }
    }
}

impl From<PersistenceError> for ApiError {
    fn from(error: PersistenceError) -> Self {
        match error {
            PersistenceError::CatalogConnectionInUse { .. } => {
                Self::conflict("catalog_connection_in_use", "wait_for_jobs_to_resolve")
            }
            PersistenceError::TableInUse { .. } => {
                Self::conflict("table_in_use", "wait_for_jobs_to_resolve")
            }
            PersistenceError::TableIdentityConflict { .. } => {
                Self::conflict("table_identity_conflict", "unregister_then_register")
            }
            PersistenceError::NotFound { .. } => Self::not_found(),
            PersistenceError::PermissionDenied { .. } => Self::new(
                StatusCode::FORBIDDEN,
                "permission_denied",
                "request_permission",
            ),
            PersistenceError::IdempotencyKeyReused { .. } => Self::new(
                StatusCode::CONFLICT,
                "idempotency_key_reused",
                "use_new_idempotency_key",
            ),
            PersistenceError::InvalidValue { .. } | PersistenceError::MissingReference { .. } => {
                Self::invalid()
            }
            PersistenceError::ConfigRevisionConflict { .. } => {
                Self::conflict("revision_conflict", "reload_and_retry")
            }
            PersistenceError::IneligibleServiceIdentity { .. } => Self::new(
                StatusCode::BAD_REQUEST,
                "service_identity_not_eligible",
                "grant_maintenance_role",
            ),
            PersistenceError::AlreadyExists { .. } => {
                Self::conflict("resource_already_exists", "choose_another_name")
            }
            PersistenceError::Database(error) => Self::from(error),
            other => {
                tracing::error!(error = %other, "api persistence failure");
                Self::internal()
            }
        }
    }
}

// Keep session authority internal; handlers cannot accept a caller-selected actor.
type Caller = axum::Extension<Session>;

/// Parses a JSON mutation body, keeping an oversized body distinguishable from
/// a malformed one.
fn json_body<T>(
    body: Result<Json<T>, axum::extract::rejection::JsonRejection>,
) -> Result<T, ApiError> {
    body.map(|Json(value)| value)
        .map_err(|error| json_rejection(&error))
}

/// Parses an optional JSON mutation body the same way as [`json_body`].
///
/// A request without a JSON content type carries no body; one that announces
/// JSON but sends nothing usable is a client error, including 413 for a body
/// over the limit.
fn json_optional_body<T>(
    body: Result<Option<Json<T>>, axum::extract::rejection::JsonRejection>,
) -> Result<Option<T>, ApiError> {
    body.map(|body| body.map(|Json(value)| value))
        .map_err(|error| json_rejection(&error))
}

fn json_rejection(error: &axum::extract::rejection::JsonRejection) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "reduce_request_size",
        )
    } else {
        ApiError::invalid()
    }
}

/// The 202 response for a durable command that created a job.
///
/// A retry adds the job it replaces; a submission's body keeps the T1 shape.
fn accepted_job(id: JobId, retry_of: Option<JobId>) -> Response {
    let url = format!("/api/v1/jobs/{id}");
    let mut body = json!({"operation_id": id, "job_id": id, "resource_url": url});
    if let Some(source) = retry_of {
        body["retry_of_job_id"] = json!(source);
    }
    (StatusCode::ACCEPTED, [(header::LOCATION, url)], Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_database_failures_are_retryable() {
        assert!(is_transient_database_error(&sqlx::Error::PoolTimedOut));
        assert!(is_transient_database_error(&sqlx::Error::PoolClosed));
        assert!(is_transient_database_error(&sqlx::Error::Io(
            std::io::Error::other("connection reset")
        )));
        assert!(!is_transient_database_error(&sqlx::Error::RowNotFound));
    }

    #[test]
    fn openapi_document_uses_the_configured_cookie_name() {
        let document = openapi_document("custom_session").expect("document is valid");
        let document: serde_json::Value =
            serde_json::from_slice(&document).expect("document is JSON");
        assert_eq!(
            document["components"]["securitySchemes"]["sessionCookie"]["name"],
            "custom_session"
        );
    }
}
