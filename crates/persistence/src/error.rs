//! Errors returned by the persistence layer.

/// An error from a repository or from database setup.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    /// The database rejected or failed a query.
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    /// A schema migration failed to apply.
    #[error("database migration failed")]
    Migration(#[from] sqlx::migrate::MigrateError),
}
