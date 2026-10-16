//! Errors returned by the persistence layer.

use bergsmith_common::ids::{CatalogConnectionId, TableId};

/// PostgreSQL `unique_violation`.
const UNIQUE_VIOLATION: &str = "23505";
/// PostgreSQL `foreign_key_violation`.
const FOREIGN_KEY_VIOLATION: &str = "23503";
/// PostgreSQL `check_violation`.
const CHECK_VIOLATION: &str = "23514";

/// An error from a repository or from database setup.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    /// The database rejected or failed a query.
    #[error("database operation failed")]
    Database(#[source] sqlx::Error),
    /// A schema migration failed to apply.
    #[error("database migration failed")]
    Migration(#[from] sqlx::migrate::MigrateError),
    /// A row with the same unique key already exists.
    #[error("{entity} already exists ({constraint})")]
    AlreadyExists {
        /// Kind of row that was being written.
        entity: &'static str,
        /// The unique constraint that was violated.
        constraint: String,
    },
    /// The row refers to a row that does not exist or belongs to another
    /// organization.
    #[error("{entity} refers to a missing row ({constraint})")]
    MissingReference {
        /// Kind of row that was being written.
        entity: &'static str,
        /// The foreign key that was violated.
        constraint: String,
    },
    /// A value failed a schema check, such as a malformed slug or URI.
    #[error("{entity} has an invalid value ({constraint})")]
    InvalidValue {
        /// Kind of row that was being written.
        entity: &'static str,
        /// The check constraint that was violated.
        constraint: String,
    },
    /// No row exists with the requested key.
    #[error("{entity} {id} was not found")]
    NotFound {
        /// Kind of row that was looked up.
        entity: &'static str,
        /// The key that was looked up.
        id: String,
    },
    /// A catalog alias already names a different table, for example after a
    /// table was dropped and recreated under the same name.
    #[error(
        "alias on connection {connection} names table {existing}, not the observed table {observed}"
    )]
    TableIdentityConflict {
        /// The connection the alias belongs to.
        connection: CatalogConnectionId,
        /// The identity the alias already resolves to.
        existing: TableId,
        /// The identity of the table that was observed under the alias.
        observed: TableId,
    },
    /// A stored value could not be decoded, which means the schema and the
    /// code disagree.
    #[error("stored {entity} has an unrecognized value {value:?}")]
    CorruptValue {
        /// Kind of value that failed to decode.
        entity: &'static str,
        /// The stored text.
        value: String,
    },
}

impl From<sqlx::Error> for PersistenceError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl PersistenceError {
    /// Classifies a write error: constraint violations become typed errors
    /// naming `entity`, and anything else stays a database error.
    pub(crate) fn from_write(entity: &'static str, error: sqlx::Error) -> Self {
        let Some(database_error) = error.as_database_error() else {
            return Self::Database(error);
        };
        let constraint = database_error.constraint().unwrap_or_default().to_owned();
        match database_error.code().as_deref() {
            Some(UNIQUE_VIOLATION) => Self::AlreadyExists { entity, constraint },
            Some(FOREIGN_KEY_VIOLATION) => Self::MissingReference { entity, constraint },
            Some(CHECK_VIOLATION) => Self::InvalidValue { entity, constraint },
            _ => Self::Database(error),
        }
    }
}
