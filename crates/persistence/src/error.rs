//! Errors returned by the persistence layer.

use bergsmith_common::ids::{AttemptId, CatalogConnectionId, JobId, PrincipalId, TableId, TaskId};
use bergsmith_common::job::{JobState, JobTransitionError};
use bergsmith_common::task::{AttemptTransitionError, TaskTransitionError};

/// PostgreSQL `unique_violation`.
const UNIQUE_VIOLATION: &str = "23505";
/// PostgreSQL `foreign_key_violation`.
const FOREIGN_KEY_VIOLATION: &str = "23503";
/// PostgreSQL `check_violation`.
const CHECK_VIOLATION: &str = "23514";

/// An error from a repository or from database setup.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    /// Connection routing cannot change while jobs may still need it.
    #[error("catalog connection {connection} has nonterminal jobs")]
    CatalogConnectionInUse {
        /// The connection whose routing must remain stable.
        connection: CatalogConnectionId,
    },
    /// Admission or execution resource budgets currently prevent dispatch.
    #[error("scheduler admission capacity is unavailable")]
    AdmissionUnavailable,
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
    /// A table still has nonterminal work, so its catalog names are kept.
    #[error("table {table} has nonterminal jobs")]
    TableInUse {
        /// The table whose alias removal was refused.
        table: TableId,
    },
    /// Another writer changed the job after the caller read it.
    #[error("job {job} is at revision {actual}, not the expected {expected}")]
    RevisionConflict {
        /// The job.
        job: JobId,
        /// The revision the caller read.
        expected: i64,
        /// The current revision.
        actual: i64,
    },
    /// Another writer changed a configuration row after the caller read it.
    #[error("{entity} {id} is at revision {actual}, not the expected {expected}")]
    ConfigRevisionConflict {
        /// Kind of row that was being updated.
        entity: &'static str,
        /// The row's identifier.
        id: String,
        /// The revision the caller read.
        expected: i64,
        /// The current revision.
        actual: i64,
    },
    /// A schedule named a principal that cannot execute maintenance.
    #[error("principal {principal} cannot execute maintenance for table {table}")]
    IneligibleServiceIdentity {
        /// The principal that was checked.
        principal: PrincipalId,
        /// The table it was meant to run against.
        table: TableId,
    },
    /// A scheduled job's maintenance policy was disabled before publication.
    #[error("job {job} belongs to a disabled maintenance policy")]
    MaintenancePolicyDisabled {
        /// The scheduled job.
        job: JobId,
    },
    /// The job's execution budget ran out before the attempt was granted.
    #[error("job {job} ran out of its execution budget")]
    ExecutionBudgetExceeded {
        /// The job.
        job: JobId,
    },
    /// The job lifecycle forbids the change.
    #[error(transparent)]
    JobTransition(#[from] JobTransitionError),
    /// The job's state does not allow the operation.
    #[error("job {job} is {state} and cannot {operation}")]
    JobNotInState {
        /// The job.
        job: JobId,
        /// Its current state.
        state: JobState,
        /// What was attempted.
        operation: &'static str,
    },
    /// An idempotency key was reused for a different operation or request.
    #[error("idempotency key {key:?} was already used for a different request")]
    IdempotencyKeyReused {
        /// The key.
        key: String,
    },
    /// The coordinator no longer holds leadership under this epoch.
    #[error("coordinator epoch {epoch} no longer holds leadership")]
    LeadershipLost {
        /// The epoch the caller held.
        epoch: i64,
    },
    /// Cancellation was requested before the commit send could be authorized.
    #[error("job {job} has a cancellation request and cannot publish")]
    CancellationRequested {
        /// The job.
        job: JobId,
    },
    /// A commit send was already authorized for the job.
    #[error("job {job} already authorized a commit request")]
    PublicationAlreadyAuthorized {
        /// The job.
        job: JobId,
    },
    /// The task lifecycle forbids the change.
    #[error(transparent)]
    TaskTransition(#[from] TaskTransitionError),
    /// The attempt lifecycle forbids the change.
    #[error(transparent)]
    AttemptTransition(#[from] AttemptTransitionError),
    /// The task has started every execution attempt it is allowed.
    #[error("task {task} has no execution attempts left")]
    RetryBudgetExhausted {
        /// The task.
        task: TaskId,
    },
    /// The caller's attempt generation is no longer current.
    #[error("attempt {attempt} is not the current generation")]
    StaleAttempt {
        /// The attempt.
        attempt: AttemptId,
    },
    /// An attempt resubmitted its accepted receipt with different content.
    #[error("task {task} already accepted a receipt with different content")]
    ReceiptConflict {
        /// The task.
        task: TaskId,
    },
    /// Deactivating the principal would leave the organization without an
    /// active administrator.
    #[error("principal {principal} is the organization's last active administrator")]
    LastAdministrator {
        /// The organization that would lose its last administrator.
        organization: bergsmith_common::ids::OrganizationId,
        /// The principal whose deactivation was refused.
        principal: PrincipalId,
    },
    /// The principal does not hold an active grant for the operation.
    #[error("principal {principal} lacks permission {permission} for this resource")]
    PermissionDenied {
        /// The principal that was checked.
        principal: PrincipalId,
        /// The stable permission name.
        permission: &'static str,
    },
    /// The session is unknown, expired, idle, revoked, or belongs to a
    /// deactivated principal.
    #[error("session rejected: {reason}")]
    SessionRejected {
        /// Stable reason: `unknown`, `deactivated`, `expired`, `idle`, or
        /// `revoked`.
        reason: &'static str,
    },
    /// The caller has too many outstanding operations of this kind.
    #[error("too many outstanding {entity} operations")]
    RateLimited {
        /// Kind of operation that is limited.
        entity: &'static str,
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
