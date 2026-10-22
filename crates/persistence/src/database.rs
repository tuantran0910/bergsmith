//! Connection pool setup and schema migrations.
//!
//! TLS is configured with the libpq-compatible URL parameters (`sslmode`,
//! `sslrootcert`, `sslcert`, `sslkey`) or the matching `PGSSL*` environment
//! variables, so one setting covers `serve`, `migrate`, and the administration
//! commands. Connections use rustls; production deployments use
//! `sslmode=verify-full` with `sslrootcert`.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};

use crate::PersistenceError;

/// Every schema migration, embedded at build time from `migrations/`.
///
/// Apply it only through the dedicated `migrate` command. Replicas never
/// migrate at startup.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// A PostgreSQL connection URL. It may embed a password, so it is redacted
/// from `Debug` output and never logged.
#[derive(Clone)]
pub struct DatabaseUrl(String);

impl DatabaseUrl {
    /// Wraps a connection URL such as `postgres://user:pass@host/db`.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self(url.into())
    }

    fn expose(&self) -> &str {
        &self.0
    }

    /// Parses the URL together with the `PG*` environment defaults.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::Database`] when the URL or one of its
    /// parameters is invalid.
    fn options(&self) -> Result<PgConnectOptions, PersistenceError> {
        Ok(PgConnectOptions::from_str(self.expose())?)
    }

    /// The TLS mode this URL selects, after `PGSSLMODE` and the URL's own
    /// `sslmode` parameter. Without either, the mode is
    /// [`DatabaseTlsMode::Prefer`], as in libpq.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::Database`] when the URL is invalid.
    pub fn tls_mode(&self) -> Result<DatabaseTlsMode, PersistenceError> {
        Ok(DatabaseTlsMode::from(self.options()?.get_ssl_mode()))
    }
}

/// How a PostgreSQL connection negotiates TLS (libpq `sslmode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatabaseTlsMode {
    /// Plaintext only.
    Disable,
    /// Plaintext first, TLS only if the server requires it.
    Allow,
    /// TLS if the server supports it, otherwise plaintext.
    Prefer,
    /// TLS without verifying the server certificate.
    Require,
    /// TLS with a server certificate signed by a trusted CA.
    VerifyCa,
    /// TLS with a trusted certificate whose name matches the host.
    VerifyFull,
}

impl DatabaseTlsMode {
    /// The `sslmode` parameter value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Allow => "allow",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }

    /// Whether the mode authenticates the server by certificate and host
    /// name, the only mode that resists an active network attacker.
    #[must_use]
    pub fn is_verified(self) -> bool {
        self == Self::VerifyFull
    }
}

impl From<PgSslMode> for DatabaseTlsMode {
    fn from(mode: PgSslMode) -> Self {
        match mode {
            PgSslMode::Disable => Self::Disable,
            PgSslMode::Allow => Self::Allow,
            PgSslMode::Prefer => Self::Prefer,
            PgSslMode::Require => Self::Require,
            PgSslMode::VerifyCa => Self::VerifyCa,
            PgSslMode::VerifyFull => Self::VerifyFull,
        }
    }
}

impl fmt::Display for DatabaseTlsMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for DatabaseUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DatabaseUrl(<redacted>)")
    }
}

/// Connection pool settings.
#[derive(Clone, Debug)]
pub struct DatabaseConfig {
    /// Where to connect.
    pub url: DatabaseUrl,
    /// Upper bound on pooled connections.
    pub max_connections: u32,
    /// How long to wait for a pooled connection before failing.
    pub acquire_timeout: Duration,
}

impl DatabaseConfig {
    /// Default settings for `url`: 10 connections and a 5 second acquire timeout.
    #[must_use]
    pub fn new(url: DatabaseUrl) -> Self {
        Self {
            url,
            max_connections: 10,
            acquire_timeout: Duration::from_secs(5),
        }
    }
}

/// A handle to the Bergsmith PostgreSQL database.
#[derive(Clone, Debug)]
pub struct Database {
    pool: PgPool,
}

impl Database {
    /// Opens a connection pool and checks that the server is reachable.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::Database`] when the URL is invalid, the
    /// TLS mode cannot be satisfied (including an untrusted or mismatched
    /// server certificate), or no connection can be established within the
    /// acquire timeout.
    pub async fn connect(config: &DatabaseConfig) -> Result<Self, PersistenceError> {
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(config.acquire_timeout)
            .connect_with(config.url.options()?)
            .await?;
        Ok(Self { pool })
    }

    /// Wraps an existing pool, for example one provided by a test harness.
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Applies every pending migration. Applying is idempotent, and
    /// concurrent runs serialize on a PostgreSQL advisory lock.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::Migration`] when a migration fails or an
    /// applied migration no longer matches its embedded checksum.
    pub async fn migrate(&self) -> Result<(), PersistenceError> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// The underlying connection pool.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_url_is_redacted_from_debug_output() {
        let url = DatabaseUrl::new("postgres://user:secret@localhost/db");
        let debug = format!("{:?}", DatabaseConfig::new(url));
        assert!(!debug.contains("secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn tls_mode_reads_the_sslmode_parameter() {
        for (value, expected) in [
            ("disable", DatabaseTlsMode::Disable),
            ("allow", DatabaseTlsMode::Allow),
            ("prefer", DatabaseTlsMode::Prefer),
            ("require", DatabaseTlsMode::Require),
            ("verify-ca", DatabaseTlsMode::VerifyCa),
            ("verify-full", DatabaseTlsMode::VerifyFull),
        ] {
            let url = DatabaseUrl::new(format!("postgres://localhost/db?sslmode={value}"));
            let mode = url.tls_mode().unwrap();
            assert_eq!(mode, expected);
            assert_eq!(mode.as_str(), value);
            assert_eq!(mode.to_string(), value);
            assert_eq!(mode.is_verified(), value == "verify-full");
        }
    }

    #[test]
    fn tls_mode_rejects_an_unknown_sslmode() {
        let url = DatabaseUrl::new("postgres://localhost/db?sslmode=sometimes");
        assert!(matches!(url.tls_mode(), Err(PersistenceError::Database(_))));
    }

    #[test]
    fn default_config_bounds_the_pool_and_acquire_wait() {
        let config = DatabaseConfig::new(DatabaseUrl::new("postgres://localhost/db"));
        assert_eq!(config.max_connections, 10);
        assert_eq!(config.acquire_timeout, Duration::from_secs(5));
    }
}
