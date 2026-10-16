//! Connection pool setup and schema migrations.

use std::fmt;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;

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
    /// Returns [`PersistenceError::Database`] when the URL is invalid or no
    /// connection can be established within the acquire timeout.
    pub async fn connect(config: &DatabaseConfig) -> Result<Self, PersistenceError> {
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(config.acquire_timeout)
            .connect(config.url.expose())
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
    fn default_config_bounds_the_pool_and_acquire_wait() {
        let config = DatabaseConfig::new(DatabaseUrl::new("postgres://localhost/db"));
        assert_eq!(config.max_connections, 10);
        assert_eq!(config.acquire_timeout, Duration::from_secs(5));
    }
}
