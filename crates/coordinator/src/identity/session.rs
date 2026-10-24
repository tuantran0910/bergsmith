//! Browser-facing sessions and CSRF tokens.
//!
//! A session token is 32 random bytes in base64url. PostgreSQL stores
//! `SHA-256("bergsmith.session-token.v1" || 0x00 || token)`, so the database
//! alone cannot mint a usable cookie. The same construction binds a second
//! token to CSRF checks: unsafe requests must present it in
//! [`super::CSRF_HEADER`], and the coordinator compares digests in
//! constant time.

use std::fmt;
use std::time::Duration;

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{OrganizationId, PrincipalId};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::auth::{self, NewSession, Session};
use sqlx::PgConnection;
use thiserror::Error;

use super::random;

const SESSION_DOMAIN: &[u8] = b"bergsmith.session-token.v1";
const CSRF_DOMAIN: &[u8] = b"bergsmith.csrf-token.v1";

/// Session and cookie policy.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// Cookie name.
    pub cookie_name: String,
    /// Whether the cookie carries `Secure` (always true outside development).
    pub secure: bool,
    /// Absolute session lifetime.
    pub absolute_lifetime: Duration,
    /// Reject a session that has not been used for this long.
    pub idle_timeout: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            cookie_name: super::SESSION_COOKIE.to_owned(),
            secure: true,
            absolute_lifetime: Duration::from_secs(12 * 60 * 60),
            idle_timeout: Duration::from_secs(2 * 60 * 60),
        }
    }
}

impl SessionConfig {
    /// Rejects a configuration that would make sessions unbounded.
    ///
    /// # Errors
    ///
    /// [`SessionError::Configuration`] when a lifetime is zero or the idle
    /// timeout exceeds the absolute lifetime.
    pub fn validate(&self) -> Result<(), SessionError> {
        if self.cookie_name.is_empty()
            || self.absolute_lifetime.is_zero()
            || self.idle_timeout.is_zero()
            || self.idle_timeout > self.absolute_lifetime
        {
            return Err(SessionError::Configuration);
        }
        Ok(())
    }
}

/// A random session token. `Debug` never prints it and there is no `Display`.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionToken(String);

impl SessionToken {
    /// Generates a new token.
    ///
    /// # Errors
    ///
    /// [`SessionError::Random`] when the operating system random source fails.
    pub fn generate() -> Result<Self, SessionError> {
        random::token().map(Self).ok_or(SessionError::Random)
    }

    /// Parses a token from a cookie.
    ///
    /// # Errors
    ///
    /// [`SessionError::Malformed`] unless it is 43 base64url characters.
    pub fn parse(value: &str) -> Result<Self, SessionError> {
        random::token_bytes(value)
            .map(|_| Self(value.to_owned()))
            .ok_or(SessionError::Malformed)
    }

    /// The stored verifier.
    #[must_use]
    pub fn verifier(&self) -> Sha256Digest {
        random::verifier(SESSION_DOMAIN, &self.0)
    }

    /// The value to put in the cookie. Exposes the secret; send it only over
    /// TLS and never log it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(<redacted>)")
    }
}

/// A CSRF token bound to one session.
#[derive(Clone, PartialEq, Eq)]
pub struct CsrfToken(String);

impl CsrfToken {
    /// Generates a new token.
    ///
    /// # Errors
    ///
    /// [`SessionError::Random`] when the operating system random source fails.
    pub fn generate() -> Result<Self, SessionError> {
        random::token().map(Self).ok_or(SessionError::Random)
    }

    /// Parses a presented token.
    ///
    /// # Errors
    ///
    /// [`SessionError::Malformed`] unless it is 43 base64url characters.
    pub fn parse(value: &str) -> Result<Self, SessionError> {
        random::token_bytes(value)
            .map(|_| Self(value.to_owned()))
            .ok_or(SessionError::Malformed)
    }

    /// The stored verifier.
    #[must_use]
    pub fn verifier(&self) -> Sha256Digest {
        random::verifier(CSRF_DOMAIN, &self.0)
    }

    /// The value returned to the client after login.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CsrfToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CsrfToken(<redacted>)")
    }
}

/// A newly issued session and its two browser tokens.
#[derive(Debug)]
pub struct IssuedSession {
    /// Session cookie value.
    pub token: SessionToken,
    /// CSRF value the client must echo on unsafe requests.
    pub csrf: CsrfToken,
    /// Persisted session row.
    pub session: Session,
}

/// Why a session operation failed.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Configuration is invalid.
    #[error("invalid session configuration")]
    Configuration,
    /// Secure randomness failed.
    #[error("secure random generation failed")]
    Random,
    /// The token is not 43 base64url characters.
    #[error("malformed session token")]
    Malformed,
    /// The session is unknown or no longer usable.
    #[error("session rejected: {reason}")]
    Rejected {
        /// Stable reason: `unknown`, `deactivated`, `expired`, `idle`, or
        /// `revoked`.
        reason: &'static str,
    },
    /// The presented CSRF token does not match the session.
    #[error("csrf token rejected")]
    Csrf,
    /// The identity store failed.
    #[error(transparent)]
    Persistence(PersistenceError),
}

impl SessionError {
    /// A stable reason code for operator logs.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Configuration => "SessionConfigurationInvalid",
            Self::Random => "SessionRandomUnavailable",
            Self::Malformed => "SessionTokenMalformed",
            Self::Rejected { .. } => "SessionRejected",
            Self::Csrf => "SessionCsrfRejected",
            Self::Persistence(_) => "SessionLedgerUnavailable",
        }
    }
}

impl From<PersistenceError> for SessionError {
    fn from(error: PersistenceError) -> Self {
        match error {
            PersistenceError::SessionRejected { reason } => Self::Rejected { reason },
            other => Self::Persistence(other),
        }
    }
}

/// Issues a session for a verified OIDC identity.
///
/// # Errors
///
/// [`SessionError::Configuration`], [`SessionError::Random`], and persistence
/// errors, including a principal that is not in the organization.
pub async fn issue(
    conn: &mut PgConnection,
    config: &SessionConfig,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    issuer: &str,
    subject: &str,
) -> Result<IssuedSession, SessionError> {
    config.validate()?;
    let token = SessionToken::generate()?;
    let csrf = CsrfToken::generate()?;
    let session = auth::create_session(
        conn,
        &NewSession {
            token_hash: token.verifier(),
            organization_id,
            principal_id,
            issuer: issuer.to_owned(),
            subject: subject.to_owned(),
            csrf_token_hash: csrf.verifier(),
            lifetime: config.absolute_lifetime,
        },
    )
    .await?;
    Ok(IssuedSession {
        token,
        csrf,
        session,
    })
}

/// Authenticates a presented session token.
///
/// # Errors
///
/// [`SessionError::Malformed`], [`SessionError::Rejected`] for an expired,
/// idle, revoked, or unknown session, and persistence errors.
pub async fn authenticate(
    conn: &mut PgConnection,
    config: &SessionConfig,
    token: &SessionToken,
) -> Result<Session, SessionError> {
    config.validate()?;
    Ok(auth::authenticate_session(conn, token.verifier(), config.idle_timeout).await?)
}

/// Requires the presented CSRF token to match the session.
///
/// # Errors
///
/// [`SessionError::Csrf`] when it does not, and [`SessionError::Malformed`]
/// when the value is not a token.
pub fn verify_csrf(session: &Session, presented: &str) -> Result<(), SessionError> {
    let presented = CsrfToken::parse(presented)?;
    if random::digests_equal(&presented.verifier(), &session.csrf_token_hash) {
        Ok(())
    } else {
        Err(SessionError::Csrf)
    }
}

/// Revokes one session. Returns whether an active session changed.
///
/// # Errors
///
/// Returns a persistence error if the update fails.
pub async fn logout(conn: &mut PgConnection, token: &SessionToken) -> Result<bool, SessionError> {
    Ok(auth::revoke_session(conn, token.verifier(), "logout").await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_redacted_and_parseable() {
        let token = SessionToken::generate().unwrap();
        assert_eq!(format!("{token:?}"), "SessionToken(<redacted>)");
        assert_eq!(SessionToken::parse(token.expose()).unwrap(), token);
        assert_eq!(
            SessionToken::parse("short").unwrap_err().reason_code(),
            "SessionTokenMalformed"
        );
        let csrf = CsrfToken::generate().unwrap();
        assert_eq!(format!("{csrf:?}"), "CsrfToken(<redacted>)");
    }

    #[test]
    fn a_csrf_token_must_match_the_session_verifier() {
        let presented = CsrfToken::generate().unwrap();
        let stored = CsrfToken::generate().unwrap();
        let session = Session {
            token_hash: Sha256Digest::of(b"test", b"session"),
            organization_id: OrganizationId::generate(),
            principal_id: PrincipalId::generate(),
            issuer: "https://idp.example".to_owned(),
            subject: "alice".to_owned(),
            csrf_token_hash: stored.verifier(),
            created_at: chrono::Utc::now(),
            last_seen_at: chrono::Utc::now(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        };
        assert!(verify_csrf(&session, presented.expose()).is_err());
        assert!(verify_csrf(&session, stored.expose()).is_ok());
        assert_eq!(
            verify_csrf(&session, "bad").unwrap_err().reason_code(),
            "SessionTokenMalformed"
        );
    }

    #[test]
    fn idle_timeout_cannot_exceed_absolute_lifetime() {
        let config = SessionConfig {
            idle_timeout: Duration::from_secs(13 * 60 * 60),
            ..SessionConfig::default()
        };
        assert!(matches!(
            config.validate(),
            Err(SessionError::Configuration)
        ));
    }
}
