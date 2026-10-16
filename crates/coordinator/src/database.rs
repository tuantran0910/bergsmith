//! Database administration commands run by the coordinator binary.

use std::time::Duration;

use bergsmith_persistence::{Database, DatabaseConfig};
pub use bergsmith_persistence::{DatabaseUrl, PersistenceError};

/// How long `migrate` waits for a database connection.
const MIGRATE_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

/// Applies every pending schema migration to the database at `url`.
///
/// This is the only supported way to change the schema. Run it once per
/// upgrade, before starting coordinators; replicas never migrate at startup.
///
/// # Errors
///
/// Returns [`PersistenceError::Database`] when the database is unreachable and
/// [`PersistenceError::Migration`] when a migration fails or an applied one
/// was modified.
pub async fn migrate(url: DatabaseUrl) -> Result<(), PersistenceError> {
    let mut config = DatabaseConfig::new(url);
    config.max_connections = 1;
    config.acquire_timeout = MIGRATE_ACQUIRE_TIMEOUT;
    Database::connect(&config).await?.migrate().await
}
