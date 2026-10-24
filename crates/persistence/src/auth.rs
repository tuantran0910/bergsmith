//! OIDC login state and server-side sessions.
//!
//! The browser holds one random session token and one CSRF token. PostgreSQL
//! holds only their domain-separated SHA-256 digests, so a database reader
//! cannot replay a session. A login state is bound to the browser that started
//! it, holds the PKCE verifier only until the code exchange, and is then
//! deleted.

use std::time::Duration;

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{OrganizationId, PrincipalId};
use bergsmith_common::secret::Secret;
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::PersistenceError;

/// One in-flight authorization-code exchange.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginState {
    /// Hash of the `state` value sent to the browser.
    pub state_hash: Sha256Digest,
    /// Organization the login belongs to.
    pub organization_id: OrganizationId,
    /// Relative path to return to after login.
    pub return_to: String,
    /// OIDC nonce bound to the ID token.
    pub nonce: String,
    /// PKCE verifier for the token request. Never logged.
    pub pkce_verifier: Secret,
    /// When the state was created (database time).
    pub created_at: DateTime<Utc>,
    /// When it stops being valid.
    pub expires_at: DateTime<Utc>,
}

/// Fields for a new login state.
#[derive(Clone, Debug)]
pub struct NewLoginState {
    /// Hash of the random `state` value.
    pub state_hash: Sha256Digest,
    /// Hash of the browser-binding value. Only the browser that started the
    /// flow can redeem the state.
    pub binding_hash: Sha256Digest,
    /// Organization the login belongs to.
    pub organization_id: OrganizationId,
    /// Relative path to return to after login.
    pub return_to: String,
    /// OIDC nonce.
    pub nonce: String,
    /// PKCE verifier.
    pub pkce_verifier: Secret,
    /// How long the state stays valid.
    pub lifetime: Duration,
}

/// An authenticated server-side session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// Hash of the browser's session token.
    pub token_hash: Sha256Digest,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Authenticated principal.
    pub principal_id: PrincipalId,
    /// Issuer recorded at issue time.
    pub issuer: String,
    /// Subject recorded at issue time.
    pub subject: String,
    /// Hash of the CSRF token bound to this session.
    pub csrf_token_hash: Sha256Digest,
    /// When the session was created (database time).
    pub created_at: DateTime<Utc>,
    /// Last request that authenticated with it.
    pub last_seen_at: DateTime<Utc>,
    /// Absolute expiry.
    pub expires_at: DateTime<Utc>,
}

/// Fields for a new session.
#[derive(Clone, Debug)]
pub struct NewSession {
    /// Hash of the random session token.
    pub token_hash: Sha256Digest,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Authenticated principal.
    pub principal_id: PrincipalId,
    /// Issuer from the verified ID token.
    pub issuer: String,
    /// Subject from the verified ID token.
    pub subject: String,
    /// Hash of the CSRF token.
    pub csrf_token_hash: Sha256Digest,
    /// Absolute session lifetime.
    pub lifetime: Duration,
}

#[derive(sqlx::FromRow)]
struct LoginStateRow {
    organization_id: Uuid,
    return_to: String,
    nonce: String,
    pkce_verifier: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    token_hash: Vec<u8>,
    organization_id: Uuid,
    principal_id: Uuid,
    issuer: String,
    subject: String,
    csrf_token_hash: Vec<u8>,
    created_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

/// A session row plus the database-time validity flags.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is one independent, SQL-computed rejection reason"
)]
#[derive(sqlx::FromRow)]
struct SessionLookup {
    token_hash: Vec<u8>,
    organization_id: Uuid,
    principal_id: Uuid,
    issuer: String,
    subject: String,
    csrf_token_hash: Vec<u8>,
    created_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    revoked: bool,
    deactivated: bool,
    expired: bool,
    idle: bool,
}

impl SessionLookup {
    /// The first rejection reason, if any, in precedence order.
    fn rejection(&self) -> Option<&'static str> {
        if self.revoked {
            Some("revoked")
        } else if self.deactivated {
            Some("deactivated")
        } else if self.expired {
            Some("expired")
        } else if self.idle {
            Some("idle")
        } else {
            None
        }
    }

    fn into_session_row(self) -> SessionRow {
        SessionRow {
            token_hash: self.token_hash,
            organization_id: self.organization_id,
            principal_id: self.principal_id,
            issuer: self.issuer,
            subject: self.subject,
            csrf_token_hash: self.csrf_token_hash,
            created_at: self.created_at,
            last_seen_at: self.last_seen_at,
            expires_at: self.expires_at,
        }
    }
}

impl TryFrom<SessionRow> for Session {
    type Error = PersistenceError;

    fn try_from(row: SessionRow) -> Result<Self, Self::Error> {
        Ok(Self {
            token_hash: Sha256Digest::from_slice(&row.token_hash).map_err(|_| {
                PersistenceError::CorruptValue {
                    entity: "session token hash",
                    value: "wrong length".to_owned(),
                }
            })?,
            organization_id: OrganizationId::from_uuid(row.organization_id),
            principal_id: PrincipalId::from_uuid(row.principal_id),
            issuer: row.issuer,
            subject: row.subject,
            csrf_token_hash: Sha256Digest::from_slice(&row.csrf_token_hash).map_err(|_| {
                PersistenceError::CorruptValue {
                    entity: "session csrf hash",
                    value: "wrong length".to_owned(),
                }
            })?,
            created_at: row.created_at,
            last_seen_at: row.last_seen_at,
            expires_at: row.expires_at,
        })
    }
}

/// The most outstanding login attempts one organization may hold at once. It
/// bounds the write amplification an unauthenticated `/auth/login` can cause.
pub const MAX_OUTSTANDING_LOGIN_STATES: i64 = 32;

/// How many rows one maintenance statement reclaims at a time.
const RECLAIM_BATCH: i64 = 500;

/// Persists a login state, reclaiming a bounded number of stale rows first.
///
/// # Errors
///
/// [`PersistenceError::MissingReference`] when the organization does not
/// exist, [`PersistenceError::InvalidValue`] for a malformed `return_to`,
/// nonce, or verifier, and [`PersistenceError::RateLimited`] when the
/// organization already has [`MAX_OUTSTANDING_LOGIN_STATES`] outstanding.
pub async fn create_login_state(
    conn: &mut PgConnection,
    new: &NewLoginState,
) -> Result<(), PersistenceError> {
    let mut tx = conn.begin().await?;
    // `now()` is stable within the statement, so PostgreSQL can use the
    // expiry index as a range; `clock_timestamp()` is volatile and forces a
    // scan even with LIMIT.
    sqlx::query(
        "DELETE FROM auth_login_states WHERE ctid IN (
             SELECT ctid FROM auth_login_states
             WHERE expires_at <= now() LIMIT $1)",
    )
    .bind(RECLAIM_BATCH)
    .execute(&mut *tx)
    .await?;
    reclaim_expired_sessions(&mut tx, Duration::from_secs(0), RECLAIM_BATCH).await?;
    // Serialize count-and-insert per organization: the row lock makes two
    // concurrent logins observe each other's state, so the cap holds.
    let organization: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM organizations WHERE id = $1 FOR UPDATE")
            .bind(new.organization_id.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
    if organization.is_none() {
        return Err(PersistenceError::NotFound {
            entity: "organization",
            id: new.organization_id.to_string(),
        });
    }
    let outstanding: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM auth_login_states
         WHERE organization_id = $1 AND expires_at > now()",
    )
    .bind(new.organization_id.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    if outstanding >= MAX_OUTSTANDING_LOGIN_STATES {
        return Err(PersistenceError::RateLimited {
            entity: "login state",
        });
    }
    sqlx::query(
        "INSERT INTO auth_login_states
             (state_hash, binding_hash, organization_id, return_to, nonce, pkce_verifier, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6,
                 clock_timestamp() + make_interval(secs => $7))",
    )
    .bind(new.state_hash.as_bytes().as_slice())
    .bind(new.binding_hash.as_bytes().as_slice())
    .bind(new.organization_id.as_uuid())
    .bind(&new.return_to)
    .bind(&new.nonce)
    .bind(new.pkce_verifier.expose())
    .bind(new.lifetime.as_secs_f64())
    .execute(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("login state", error))?;
    tx.commit().await?;
    Ok(())
}

/// Atomically consumes the login state of the browser that started it.
///
/// The row is deleted in the same statement, so the PKCE verifier no longer
/// exists once it has been used. A replayed, foreign, expired, or unknown
/// `state` all return [`PersistenceError::NotFound`], so the caller cannot
/// distinguish them.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] for an unusable state, and database errors.
pub async fn consume_login_state(
    conn: &mut PgConnection,
    state_hash: Sha256Digest,
    binding_hash: Sha256Digest,
) -> Result<LoginState, PersistenceError> {
    let row = sqlx::query_as::<_, LoginStateRow>(
        "DELETE FROM auth_login_states
         WHERE state_hash = $1 AND binding_hash = $2 AND expires_at > clock_timestamp()
         RETURNING organization_id, return_to, nonce, pkce_verifier, created_at, expires_at",
    )
    .bind(state_hash.as_bytes().as_slice())
    .bind(binding_hash.as_bytes().as_slice())
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "login state",
        id: state_hash.to_hex(),
    })?;
    Ok(LoginState {
        state_hash,
        organization_id: OrganizationId::from_uuid(row.organization_id),
        return_to: row.return_to,
        nonce: row.nonce,
        pkce_verifier: Secret::new(row.pkce_verifier),
        created_at: row.created_at,
        expires_at: row.expires_at,
    })
}

/// Deletes a bounded batch of sessions that expired at least `retain` ago.
///
/// Returns how many rows changed. Callers bound the batch so reclamation never
/// becomes an unbounded statement on a request or scheduling path.
///
/// # Errors
///
/// Returns a database error if the delete fails.
pub async fn reclaim_expired_sessions(
    conn: &mut PgConnection,
    retain: Duration,
    limit: i64,
) -> Result<u64, PersistenceError> {
    let result = sqlx::query(
        "DELETE FROM auth_sessions WHERE ctid IN (
             SELECT ctid FROM auth_sessions
             WHERE expires_at <= now() - make_interval(secs => $1)
             LIMIT $2)",
    )
    .bind(retain.as_secs_f64())
    .bind(limit)
    .execute(conn)
    .await?;
    Ok(result.rows_affected())
}

/// Creates a session.
///
/// # Errors
///
/// [`PersistenceError::AlreadyExists`] for a duplicate token hash,
/// [`PersistenceError::MissingReference`] when the principal is not in the
/// organization, and database errors.
pub async fn create_session(
    conn: &mut PgConnection,
    new: &NewSession,
) -> Result<Session, PersistenceError> {
    let row = sqlx::query_as::<_, SessionRow>(
        "INSERT INTO auth_sessions
             (token_hash, organization_id, principal_id, issuer, subject,
              csrf_token_hash, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6,
                 clock_timestamp() + make_interval(secs => $7))
         RETURNING token_hash, organization_id, principal_id, issuer, subject,
                   csrf_token_hash, created_at, last_seen_at, expires_at",
    )
    .bind(new.token_hash.as_bytes().as_slice())
    .bind(new.organization_id.as_uuid())
    .bind(new.principal_id.as_uuid())
    .bind(&new.issuer)
    .bind(&new.subject)
    .bind(new.csrf_token_hash.as_bytes().as_slice())
    .bind(new.lifetime.as_secs_f64())
    .fetch_one(conn)
    .await
    .map_err(|error| PersistenceError::from_write("session", error))?;
    row.try_into()
}

/// Authenticates a session token, refreshing its idle timestamp at most once
/// per [`TOUCH_INTERVAL`].
///
/// The row must still belong to an active principal whose issuer and subject
/// match the session's binding. An expired, revoked, or idle session, or one
/// whose principal was deactivated, is rejected with a typed reason that never
/// includes the token. The read takes no row lock, so
/// requests that share a session do not serialize; a revocation that lands
/// concurrently applies to the next request.
///
/// # Errors
///
/// [`PersistenceError::SessionRejected`] and database errors.
pub async fn authenticate_session(
    conn: &mut PgConnection,
    token_hash: Sha256Digest,
    idle_timeout: Duration,
) -> Result<Session, PersistenceError> {
    let row = lookup_session(conn, token_hash, idle_timeout).await?;
    // Coarse, conditional touch: one write per session per touch interval
    // instead of one per request, and never a lock. The interval is capped at
    // half the idle timeout, so a short timeout still refreshes before it
    // expires.
    let touch = TOUCH_INTERVAL
        .min(idle_timeout / 2)
        .max(Duration::from_millis(1));
    sqlx::query(
        "UPDATE auth_sessions SET last_seen_at = clock_timestamp()
         WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > clock_timestamp()
           AND last_seen_at < clock_timestamp() - make_interval(secs => $3)",
    )
    .bind(token_hash.as_bytes().as_slice())
    .bind(idle_timeout.as_secs_f64())
    .bind(touch.as_secs_f64())
    .execute(&mut *conn)
    .await?;
    Session::try_from(row.into_session_row())
}

/// Checks that a session is still valid without refreshing its idle
/// timestamp.
///
/// A long-lived response such as an event stream uses this to notice
/// revocation, expiry, or idleness. Holding a stream open is not activity, so
/// it never extends an idle session.
///
/// # Errors
///
/// [`PersistenceError::SessionRejected`] and database errors.
pub async fn verify_session(
    conn: &mut PgConnection,
    token_hash: Sha256Digest,
    idle_timeout: Duration,
) -> Result<(), PersistenceError> {
    lookup_session(conn, token_hash, idle_timeout)
        .await
        .map(|_| ())
}

/// Reads a session and rejects it when revoked, expired, or idle.
async fn lookup_session(
    conn: &mut PgConnection,
    token_hash: Sha256Digest,
    idle_timeout: Duration,
) -> Result<SessionLookup, PersistenceError> {
    let row = sqlx::query_as::<_, SessionLookup>(
        "SELECT s.token_hash, s.organization_id, s.principal_id, s.issuer, s.subject,
                s.csrf_token_hash, s.created_at, s.last_seen_at, s.expires_at,
                s.revoked_at IS NOT NULL AS revoked,
                p.deactivated_at IS NOT NULL AS deactivated,
                s.expires_at <= clock_timestamp() AS expired,
                s.last_seen_at <= clock_timestamp() - make_interval(secs => $2) AS idle
         FROM auth_sessions s
         JOIN principals p
           ON p.organization_id = s.organization_id AND p.id = s.principal_id
          AND p.issuer = s.issuer AND p.subject = s.subject
         WHERE s.token_hash = $1",
    )
    .bind(token_hash.as_bytes().as_slice())
    .bind(idle_timeout.as_secs_f64())
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(PersistenceError::SessionRejected { reason: "unknown" })?;
    if let Some(reason) = row.rejection() {
        return Err(PersistenceError::SessionRejected { reason });
    }
    Ok(row)
}

/// Upper bound on how often an authenticated request refreshes a session's idle
/// timestamp. The effective interval is the smaller of this and half the idle
/// timeout.
pub const TOUCH_INTERVAL: Duration = Duration::from_secs(30);

/// Returns the organization a session token belongs to, including a revoked or
/// expired session, so a denial can still be attributed.
///
/// # Errors
///
/// Returns a database error if the lookup fails.
pub async fn session_organization(
    conn: &mut PgConnection,
    token_hash: Sha256Digest,
) -> Result<Option<OrganizationId>, PersistenceError> {
    let organization: Option<Uuid> =
        sqlx::query_scalar("SELECT organization_id FROM auth_sessions WHERE token_hash = $1")
            .bind(token_hash.as_bytes().as_slice())
            .fetch_optional(conn)
            .await?;
    Ok(organization.map(OrganizationId::from_uuid))
}

/// Revokes one session. Returns whether an active session changed.
///
/// # Errors
///
/// Returns a database error if the update fails.
pub async fn revoke_session(
    conn: &mut PgConnection,
    token_hash: Sha256Digest,
    reason: &str,
) -> Result<bool, PersistenceError> {
    let result = sqlx::query(
        "UPDATE auth_sessions SET revoked_at = clock_timestamp(), revoked_reason = $2
         WHERE token_hash = $1 AND revoked_at IS NULL",
    )
    .bind(token_hash.as_bytes().as_slice())
    .bind(reason)
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Revokes every active session of a principal. Returns how many changed.
///
/// # Errors
///
/// Returns a database error if the update fails.
pub async fn revoke_principal_sessions(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    reason: &str,
) -> Result<u64, PersistenceError> {
    let result = sqlx::query(
        "UPDATE auth_sessions SET revoked_at = clock_timestamp(), revoked_reason = $3
         WHERE organization_id = $1 AND principal_id = $2 AND revoked_at IS NULL",
    )
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(reason)
    .execute(conn)
    .await?;
    Ok(result.rows_affected())
}
