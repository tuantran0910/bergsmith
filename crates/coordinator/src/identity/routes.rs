//! The `/auth` HTTP routes.
//!
//! Four endpoints:
//!
//! | Route | Purpose |
//! |---|---|
//! | `GET /auth/login` | Start the authorization-code flow for one organization |
//! | `GET /auth/callback` | Consume the login state, verify the ID token, issue a session |
//! | `POST /auth/logout` | Revoke the session; requires the CSRF header |
//! | `GET /auth/session` | Describe the current session |
//!
//! The login state is bound to the browser that started it by a second cookie,
//! so a captured `state` cannot be redeemed in another browser. The session
//! cookie is `HttpOnly` and `SameSite=Lax`; the CSRF cookie is readable by the
//! application so it can echo the value in [`CSRF_HEADER`]. Neither cookie
//! carries authority by itself: the session row is the authority and the CSRF
//! value is checked against its stored digest.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bergsmith_common::audit::{ActorKind, AuditAction, AuditOutcome};
use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::OrganizationId;
use bergsmith_common::permission::{GrantScope, Permission};
use bergsmith_persistence::grants::RoleGrant;
use bergsmith_persistence::tenancy::PrincipalKind;
use bergsmith_persistence::{Database, PersistenceError, tenancy};
use serde::Deserialize;
use serde_json::json;
use sqlx::Connection;

use super::audit;
use super::oidc::OidcProvider;
use super::session::{self, SessionConfig, SessionToken};
use super::{CSRF_HEADER, SESSION_COOKIE};

/// The CSRF cookie the browser-side application reads.
pub const CSRF_COOKIE: &str = "bergsmith_csrf";

/// The cookie that binds a login attempt to the browser that started it.
pub const LOGIN_COOKIE: &str = "bergsmith_login";

const LOGIN_STATE_DOMAIN: &[u8] = b"bergsmith.login-state.v1";
const LOGIN_BINDING_DOMAIN: &[u8] = b"bergsmith.login-binding.v1";
const DEFAULT_RETURN_TO: &str = "/";
const MAX_RETURN_TO: usize = 2048;

/// Shared state for the auth routes.
#[derive(Clone)]
pub struct IdentityState {
    /// Durable sessions, login states, principals, and audit rows.
    pub database: Database,
    /// Discovered OIDC provider.
    pub provider: Arc<OidcProvider>,
    /// Session and cookie policy.
    pub sessions: SessionConfig,
    /// How long a login may stay between redirect and callback.
    pub login_lifetime: Duration,
}

impl std::fmt::Debug for IdentityState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdentityState")
            .field("provider", &self.provider)
            .field("sessions", &self.sessions)
            .field("login_lifetime", &self.login_lifetime)
            .finish_non_exhaustive()
    }
}

impl IdentityState {
    /// Creates state with a ten-minute login window.
    #[must_use]
    pub fn new(database: Database, provider: Arc<OidcProvider>, sessions: SessionConfig) -> Self {
        Self {
            database,
            provider,
            sessions,
            login_lifetime: Duration::from_secs(10 * 60),
        }
    }
}

/// Builds the `/auth` router.
pub fn router(state: IdentityState) -> Router {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", post(logout))
        .route("/auth/session", get(session_info))
        .with_state(state)
}

#[derive(Deserialize)]
struct LoginParams {
    organization: String,
    return_to: Option<String>,
}

#[derive(Deserialize)]
struct CallbackParams {
    code: String,
    state: String,
}

/// A client-safe error: a status and a stable code, never a token or claim.
#[derive(Debug)]
struct HttpError {
    status: StatusCode,
    code: &'static str,
}

impl HttpError {
    const fn unauthorized(code: &'static str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code,
        }
    }

    const fn bad_request(code: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
        }
    }

    const fn unavailable() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "identity_unavailable",
        }
    }

    const fn rate_limited() -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "rate_limited",
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(json!({ "error": self.code }))).into_response();
        no_store(&mut response);
        response
    }
}

/// Maps a ledger error to a client-safe status. Only a genuine `NotFound`
/// becomes `not_found`; an outage or an unexpected failure is a 503, so a
/// database problem is never reported as a client mistake.
fn ledger_error(error: &PersistenceError, not_found: HttpError) -> HttpError {
    match error {
        PersistenceError::NotFound { .. } => not_found,
        PersistenceError::RateLimited { .. } => HttpError::rate_limited(),
        _ => HttpError::unavailable(),
    }
}

/// Maps a session error: a rejected session is a client error, an unavailable
/// ledger is not.
fn session_error(error: &session::SessionError) -> HttpError {
    match error {
        session::SessionError::Persistence(_) | session::SessionError::Random => {
            HttpError::unavailable()
        }
        session::SessionError::Configuration
        | session::SessionError::Malformed
        | session::SessionError::Rejected { .. }
        | session::SessionError::Csrf => HttpError::unauthorized("invalid_session"),
    }
}

async fn login(
    State(state): State<IdentityState>,
    Query(params): Query<LoginParams>,
) -> Result<Response, HttpError> {
    let return_to = validated_return_to(params.return_to.as_deref())
        .ok_or_else(|| HttpError::bad_request("invalid_return_to"))?;
    let mut conn = state
        .database
        .pool()
        .acquire()
        .await
        .map_err(|_| HttpError::unavailable())?;
    let organization = tenancy::organization_by_slug(&mut conn, &params.organization)
        .await
        .map_err(|error| ledger_error(&error, HttpError::bad_request("unknown_organization")))?;
    let login_state = super::random::token().ok_or_else(HttpError::unavailable)?;
    let binding = super::random::token().ok_or_else(HttpError::unavailable)?;
    let nonce = super::random::token().ok_or_else(HttpError::unavailable)?;
    let pkce = super::oidc::Pkce::generate().map_err(|_| HttpError::unavailable())?;
    bergsmith_persistence::auth::create_login_state(
        &mut conn,
        &bergsmith_persistence::auth::NewLoginState {
            state_hash: Sha256Digest::of(LOGIN_STATE_DOMAIN, login_state.as_bytes()),
            binding_hash: Sha256Digest::of(LOGIN_BINDING_DOMAIN, binding.as_bytes()),
            organization_id: organization.id,
            return_to: return_to.clone(),
            nonce: nonce.clone(),
            pkce_verifier: pkce.verifier().clone(),
            lifetime: state.login_lifetime,
        },
    )
    .await
    .map_err(|error| {
        tracing::warn!(error = %error, "login state could not be persisted");
        ledger_error(&error, HttpError::bad_request("invalid_login_request"))
    })?;
    drop(conn);
    let url = state
        .provider
        .authorization_url(&login_state, &nonce, &pkce)
        .map_err(|_| HttpError::unavailable())?;
    let mut response = Redirect::to(url.as_str()).into_response();
    append_cookie_headers(
        &mut response,
        &[login_cookie(
            &state.sessions,
            state.login_lifetime,
            &binding,
        )],
    );
    no_store(&mut response);
    Ok(response)
}

async fn callback(
    State(state): State<IdentityState>,
    headers: HeaderMap,
    Query(params): Query<CallbackParams>,
) -> Result<Response, HttpError> {
    let Some(binding) = cookie(&headers, LOGIN_COOKIE) else {
        audit_session_denial(&state, None, None, "missing_login_binding").await;
        return Err(HttpError::unauthorized("invalid_login_state"));
    };
    let state_hash = Sha256Digest::of(LOGIN_STATE_DOMAIN, params.state.as_bytes());
    let binding_hash = Sha256Digest::of(LOGIN_BINDING_DOMAIN, binding.as_bytes());
    // Consume the state on its own short-lived connection, then release it
    // before the token exchange performs network I/O.
    let login = {
        let mut conn = state
            .database
            .pool()
            .acquire()
            .await
            .map_err(|_| HttpError::unavailable())?;
        match bergsmith_persistence::auth::consume_login_state(&mut conn, state_hash, binding_hash)
            .await
        {
            Ok(login) => login,
            Err(error) => {
                tracing::warn!(reason = %error, "login state was rejected");
                if let Err(audit_error) =
                    audit::session_denied(&mut conn, None, None, "invalid_login_state").await
                {
                    tracing::warn!(
                        reason = audit_error.reason_code(),
                        "session-denial audit record failed"
                    );
                }
                return Err(ledger_error(
                    &error,
                    HttpError::unauthorized("invalid_login_state"),
                ));
            }
        }
    };
    let identity = exchange_code(&state, &login, &params.code).await?;
    let mut conn = state
        .database
        .pool()
        .acquire()
        .await
        .map_err(|_| HttpError::unavailable())?;
    let issued = begin_session(&state, &mut conn, &login, &identity).await?;
    drop(conn);
    let mut response = Redirect::to(&login.return_to).into_response();
    append_cookie_headers(
        &mut response,
        &[
            session_cookie(&state.sessions, issued.token.expose()),
            csrf_cookie(&state.sessions, issued.csrf.expose()),
            clear_cookie(&state.sessions, LOGIN_COOKIE),
        ],
    );
    no_store(&mut response);
    Ok(response)
}

/// Exchanges the authorization code without a database connection held, and
/// records a failure on a fresh short-lived connection.
async fn exchange_code(
    state: &IdentityState,
    login: &bergsmith_persistence::auth::LoginState,
    code: &str,
) -> Result<super::oidc::OidcIdentity, HttpError> {
    match state
        .provider
        .exchange_code(code, &login.pkce_verifier, &login.nonce)
        .await
    {
        Ok(identity) => Ok(identity),
        Err(error) => {
            audit_login_failure(
                state,
                Some(login.organization_id),
                Some(state.provider.issuer()),
                error.reason_code(),
            )
            .await;
            Err(HttpError::unauthorized("login_failed"))
        }
    }
}

/// Resolves the provisioned interactive principal, keeps its display name in
/// step with the identity provider, and issues its session.
///
/// Login never creates rights, a service account is not an interactive
/// identity, and a deactivated principal is refused, so none of them receives
/// a session.
async fn begin_session(
    state: &IdentityState,
    conn: &mut sqlx::PgConnection,
    login: &bergsmith_persistence::auth::LoginState,
    identity: &super::oidc::OidcIdentity,
) -> Result<session::IssuedSession, HttpError> {
    // One short transaction holds a share lock on the principal from the
    // active check through session insertion. Deactivation updates the row and
    // then revokes sessions, so either it waits for this transaction (and its
    // revocation sees the new session) or it committed first and this read
    // observes the deactivated row. The OIDC exchange is already complete; no
    // network call happens inside the transaction.
    let mut tx = conn.begin().await.map_err(|_| HttpError::unavailable())?;
    let principal = match tenancy::principal_by_subject_for_share(
        &mut tx,
        login.organization_id,
        &identity.issuer,
        &identity.subject,
    )
    .await
    {
        Ok(principal) if principal.kind == PrincipalKind::User && principal.is_active() => {
            principal
        }
        Ok(principal) => {
            let reason = if principal.kind == PrincipalKind::User {
                "identity_deactivated"
            } else {
                "identity_not_interactive"
            };
            if tx.rollback().await.is_err() {
                tracing::warn!("login transaction rollback failed");
            }
            let _ = audit::login_failed(
                conn,
                Some(login.organization_id),
                Some(&identity.issuer),
                reason,
            )
            .await;
            return Err(HttpError::unauthorized("identity_not_provisioned"));
        }
        Err(error) => {
            if tx.rollback().await.is_err() {
                tracing::warn!("login transaction rollback failed");
            }
            if matches!(error, PersistenceError::NotFound { .. }) {
                let _ = audit::login_failed(
                    conn,
                    Some(login.organization_id),
                    Some(&identity.issuer),
                    "identity_not_provisioned",
                )
                .await;
            }
            return Err(ledger_error(
                &error,
                HttpError::unauthorized("identity_not_provisioned"),
            ));
        }
    };
    let issued = match session::issue(
        &mut tx,
        &state.sessions,
        login.organization_id,
        principal.id,
        &identity.issuer,
        &identity.subject,
    )
    .await
    {
        Ok(issued) => issued,
        Err(error) => {
            tracing::warn!(reason = error.reason_code(), "session could not be issued");
            return Err(session_error(&error));
        }
    };
    tx.commit().await.map_err(|_| HttpError::unavailable())?;
    // The provider owns the display name. This runs after the session
    // transaction so a refresh failure cannot abort the issuance, and a
    // failure must not block a valid sign-in: the stored name stays until the
    // next successful login.
    if principal.display_name != identity.display_name
        && let Err(error) = tenancy::update_principal_display_name(
            conn,
            login.organization_id,
            principal.id,
            &bounded_display_name(&identity.display_name),
        )
        .await
    {
        tracing::warn!(error = %error, "principal display name could not be refreshed");
    }
    if let Err(error) =
        audit::login_succeeded(conn, login.organization_id, principal.id, &identity.issuer).await
    {
        tracing::warn!(reason = error.reason_code(), "login audit record failed");
    }
    Ok(issued)
}

/// Clamps a provider-chosen display name to what the ledger accepts.
fn bounded_display_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "unknown".to_owned();
    }
    trimmed.chars().take(200).collect()
}

async fn logout(
    State(state): State<IdentityState>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let Some(raw) = cookie(&headers, &state.sessions.cookie_name) else {
        return Err(HttpError::unauthorized("no_session"));
    };
    let Ok(token) = SessionToken::parse(&raw) else {
        audit_session_denial(&state, None, None, "malformed_session").await;
        return Err(HttpError::unauthorized("invalid_session"));
    };
    let mut conn = state
        .database
        .pool()
        .acquire()
        .await
        .map_err(|_| HttpError::unavailable())?;
    let session = match session::authenticate(&mut conn, &state.sessions, &token).await {
        Ok(session) => session,
        Err(error) => {
            let organization =
                bergsmith_persistence::auth::session_organization(&mut conn, token.verifier())
                    .await
                    .ok()
                    .flatten();
            let _ = audit::session_denied(&mut conn, organization, None, "invalid_session").await;
            return Err(session_error(&error));
        }
    };
    let Some(presented) = headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
    else {
        let _ = audit::session_denied(
            &mut conn,
            Some(session.organization_id),
            Some(session.principal_id),
            "missing_csrf",
        )
        .await;
        return Err(HttpError::unauthorized("missing_csrf"));
    };
    if session::verify_csrf(&session, presented).is_err() {
        let _ = audit::session_denied(
            &mut conn,
            Some(session.organization_id),
            Some(session.principal_id),
            "invalid_csrf",
        )
        .await;
        return Err(HttpError::unauthorized("invalid_csrf"));
    }
    session::logout(&mut conn, &token)
        .await
        .map_err(|error| session_error(&error))?;
    if let Err(error) =
        audit::logged_out(&mut conn, session.organization_id, session.principal_id).await
    {
        tracing::warn!(reason = error.reason_code(), "logout audit record failed");
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    append_cookie_headers(
        &mut response,
        &[
            clear_cookie(&state.sessions, &state.sessions.cookie_name),
            clear_cookie(&state.sessions, CSRF_COOKIE),
        ],
    );
    no_store(&mut response);
    Ok(response)
}

async fn session_info(
    State(state): State<IdentityState>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let Some(raw) = cookie(&headers, &state.sessions.cookie_name) else {
        return Err(HttpError::unauthorized("no_session"));
    };
    let Ok(token) = SessionToken::parse(&raw) else {
        audit_session_denial(&state, None, None, "malformed_session").await;
        return Err(HttpError::unauthorized("invalid_session"));
    };
    let mut conn = state
        .database
        .pool()
        .acquire()
        .await
        .map_err(|_| HttpError::unavailable())?;
    let session = match session::authenticate(&mut conn, &state.sessions, &token).await {
        Ok(session) => session,
        Err(error) => {
            let organization =
                bergsmith_persistence::auth::session_organization(&mut conn, token.verifier())
                    .await
                    .ok()
                    .flatten();
            let _ = audit::session_denied(&mut conn, organization, None, "invalid_session").await;
            return Err(session_error(&error));
        }
    };
    let principal = tenancy::principal(&mut conn, session.organization_id, session.principal_id)
        .await
        .map_err(|error| ledger_error(&error, HttpError::unauthorized("invalid_session")))?;
    let grants = super::authorization::Authorizer::grants(
        &mut conn,
        session.organization_id,
        session.principal_id,
    )
    .await
    .map_err(|_| HttpError::unavailable())?;
    let permissions = effective_permissions(&grants);
    let mut response = Json(json!({
        "organization_id": session.organization_id.to_string(),
        "principal_id": session.principal_id.to_string(),
        "display_name": principal.display_name,
        "expires_at": session.expires_at.to_rfc3339(),
        "grants": grants.iter().map(|grant| json!({
            "role": grant.role.as_str(),
            "scope": grant.scope,
        })).collect::<Vec<_>>(),
        "permissions": permissions.iter().map(|(permission, scope)| json!({
            "permission": permission.as_str(),
            "scope": scope,
        })).collect::<Vec<_>>(),
    }))
    .into_response();
    no_store(&mut response);
    Ok(response)
}

/// Expands active grants into the permissions the session may exercise, each
/// with the scope its grant covers.
///
/// The pairs are sorted and deduplicated, so two roles that share a permission
/// over the same scope produce one entry. This projection only drives the
/// browser's navigation and action affordances; every API route and command
/// re-checks the ledger, and it is recomputed on each session read so a
/// revocation or deactivation shows up without signing in again.
fn effective_permissions(grants: &[RoleGrant]) -> Vec<(Permission, GrantScope)> {
    let mut held = BTreeSet::new();
    for grant in grants {
        for permission in grant.role.permissions() {
            held.insert((permission, grant.scope));
        }
    }
    held.into_iter().collect()
}

/// Records a rejected session or login state on its own short-lived
/// connection, for denials that happen before one is acquired.
async fn audit_session_denial(
    state: &IdentityState,
    organization_id: Option<OrganizationId>,
    actor: Option<bergsmith_common::ids::PrincipalId>,
    reason: &str,
) {
    let Ok(mut conn) = state.database.pool().acquire().await else {
        return;
    };
    if let Err(error) = audit::session_denied(&mut conn, organization_id, actor, reason).await {
        tracing::warn!(
            reason = error.reason_code(),
            "session-denial audit record failed"
        );
    }
}

/// Records a failed login on its own short-lived connection, so an audit write
/// never stretches a request's connection hold.
async fn audit_login_failure(
    state: &IdentityState,
    organization_id: Option<OrganizationId>,
    issuer: Option<&str>,
    reason: &str,
) {
    let Ok(mut conn) = state.database.pool().acquire().await else {
        return;
    };
    if let Err(error) = audit::login_failed(&mut conn, organization_id, issuer, reason).await {
        tracing::warn!(reason = error.reason_code(), "login audit record failed");
    }
}

fn validated_return_to(value: Option<&str>) -> Option<String> {
    let value = value.unwrap_or(DEFAULT_RETURN_TO);
    if value.is_empty()
        || value.len() > MAX_RETURN_TO
        || !value.starts_with('/')
        || value.starts_with("//")
        || value.contains('\\')
        || value.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some(value.to_owned())
}

pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    for value in headers.get_all(header::COOKIE) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for pair in value.split(';') {
            let pair = pair.trim();
            if let Some((key, value)) = pair.split_once('=')
                && key == name
            {
                return Some(value.to_owned());
            }
        }
    }
    None
}

fn append_cookie_headers(response: &mut Response, cookies: &[String]) {
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(cookie) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
}

fn no_store(response: &mut Response) {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

fn session_cookie(config: &SessionConfig, token: &str) -> String {
    format!(
        "{}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
        config.cookie_name,
        config.absolute_lifetime.as_secs(),
        if config.secure { "; Secure" } else { "" }
    )
}

fn csrf_cookie(config: &SessionConfig, token: &str) -> String {
    format!(
        "{CSRF_COOKIE}={token}; Path=/; SameSite=Lax; Max-Age={}{}",
        config.absolute_lifetime.as_secs(),
        if config.secure { "; Secure" } else { "" }
    )
}

fn login_cookie(config: &SessionConfig, lifetime: Duration, binding: &str) -> String {
    format!(
        "{LOGIN_COOKIE}={binding}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
        lifetime.as_secs(),
        if config.secure { "; Secure" } else { "" }
    )
}

fn clear_cookie(config: &SessionConfig, name: &str) -> String {
    format!(
        "{name}=; Path=/; SameSite=Lax; Max-Age=0{}",
        if config.secure { "; Secure" } else { "" }
    )
}

/// Records a denied command at an application boundary.
///
/// BS-018's handlers call this before returning `403` so denied actions stay
/// attributable even though no durable command ran. A request with no
/// authenticated actor is recorded as `Anonymous`, not `System`.
///
/// # Errors
///
/// [`audit::AuditError`] when the audit ledger is unavailable.
pub async fn audit_denied_command(
    conn: &mut sqlx::PgConnection,
    organization_id: OrganizationId,
    actor: Option<bergsmith_common::ids::PrincipalId>,
    action: AuditAction,
    resource_type: &str,
    resource_id: Option<&str>,
    reason: &str,
) -> Result<(), audit::AuditError> {
    audit::record(
        conn,
        Some(organization_id),
        actor,
        if actor.is_some() {
            ActorKind::User
        } else {
            ActorKind::Anonymous
        },
        action,
        resource_type,
        resource_id,
        AuditOutcome::Denied,
        json!({ "reason": reason }),
    )
    .await
}

/// The cookie name used by tests and documentation.
#[must_use]
pub const fn session_cookie_name() -> &'static str {
    SESSION_COOKIE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_to_accepts_only_relative_paths() {
        assert_eq!(validated_return_to(None), Some("/".to_owned()));
        assert_eq!(
            validated_return_to(Some("/tables?x=1")),
            Some("/tables?x=1".to_owned())
        );
        for rejected in [
            "",
            "tables",
            "//evil.example",
            "https://evil.example",
            "/\n",
            "/\\evil.example", // browsers normalize `\` to `/`
            "/path with space",
            "/path\t",
        ] {
            assert_eq!(validated_return_to(Some(rejected)), None, "{rejected:?}");
        }
        assert_eq!(
            validated_return_to(Some(&format!("/{}", "a".repeat(3000)))),
            None
        );
    }

    #[test]
    fn cookies_are_http_only_and_samesite_lax() {
        let config = SessionConfig::default();
        let session = session_cookie(&config, "token");
        assert!(session.contains("HttpOnly"));
        assert!(session.contains("SameSite=Lax"));
        assert!(session.contains("Secure"));
        assert!(session.contains("Path=/"));

        let csrf = csrf_cookie(&config, "csrf");
        assert!(!csrf.contains("HttpOnly"));
        assert!(csrf.contains("SameSite=Lax"));

        let login = login_cookie(&config, Duration::from_secs(600), "binding");
        assert!(login.contains("HttpOnly"));
        assert!(login.contains("SameSite=Lax"));
        assert!(login.contains("Max-Age=600"));
    }

    #[test]
    fn cookie_parsing_ignores_unrelated_pairs() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("other=1; bergsmith_session=abc; third=2"),
        );
        assert_eq!(
            cookie(&headers, "bergsmith_session").as_deref(),
            Some("abc")
        );
        assert_eq!(cookie(&headers, "missing"), None);
    }
}
