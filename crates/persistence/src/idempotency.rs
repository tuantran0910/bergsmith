//! Idempotency keys for public commands.
//!
//! A key is unique within its organization and bound to the operation and
//! request hash that first used it. Replaying the same request returns the
//! original resource; reusing the key for a different request is an error.
//!
//! Usage: call [`check`] before doing any work. For a new key, create the
//! resource and call [`record`] in the same transaction. If [`record`]
//! reports a replay, a concurrent request won: roll back and return its
//! resource.

use bergsmith_common::ids::OrganizationId;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::PersistenceError;

/// An idempotency key presented with a request.
#[derive(Clone, Debug)]
pub struct IdempotencyKey {
    /// The organization the request acts in. Keys are scoped to it.
    pub organization_id: OrganizationId,
    /// The client-supplied key, 1 to 255 characters.
    pub key: String,
    /// The command the request performs, such as `SubmitJob`.
    pub operation: String,
    /// Lowercase hex SHA-256 of the canonical request.
    pub request_hash: String,
}

/// What a key means for the current request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdempotencyStatus {
    /// The key has not been used; perform the request.
    New,
    /// The same request already ran and created this resource.
    Replay {
        /// The resource the original request created.
        resource_id: Uuid,
    },
}

/// The result of [`record`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordOutcome {
    /// The key now points at the new resource.
    Recorded,
    /// A concurrent identical request recorded the key first.
    Replay {
        /// The resource the winning request created.
        resource_id: Uuid,
    },
}

#[derive(sqlx::FromRow)]
struct KeyRow {
    operation: String,
    request_hash: String,
    resource_id: Uuid,
}

async fn lookup(
    conn: &mut PgConnection,
    key: &IdempotencyKey,
) -> Result<Option<KeyRow>, PersistenceError> {
    Ok(sqlx::query_as::<_, KeyRow>(
        "SELECT operation, request_hash, resource_id FROM idempotency_keys
         WHERE organization_id = $1 AND key = $2",
    )
    .bind(key.organization_id.as_uuid())
    .bind(&key.key)
    .fetch_optional(conn)
    .await?)
}

fn replayed(key: &IdempotencyKey, row: &KeyRow) -> Result<Uuid, PersistenceError> {
    if row.operation == key.operation && row.request_hash == key.request_hash {
        Ok(row.resource_id)
    } else {
        Err(PersistenceError::IdempotencyKeyReused {
            key: key.key.clone(),
        })
    }
}

/// Reports whether `key` is new or replays an earlier identical request.
///
/// # Errors
///
/// [`PersistenceError::IdempotencyKeyReused`] when the key was used for a
/// different operation or request.
pub async fn check(
    conn: &mut PgConnection,
    key: &IdempotencyKey,
) -> Result<IdempotencyStatus, PersistenceError> {
    match lookup(conn, key).await? {
        None => Ok(IdempotencyStatus::New),
        Some(row) => Ok(IdempotencyStatus::Replay {
            resource_id: replayed(key, &row)?,
        }),
    }
}

/// Binds `key` to the resource the request created. Call it in the same
/// transaction that created the resource.
///
/// # Errors
///
/// - [`PersistenceError::IdempotencyKeyReused`] when a concurrent request
///   recorded the key for a different operation or request.
/// - [`PersistenceError::InvalidValue`] for an empty or overlong key or a
///   malformed request hash.
/// - [`PersistenceError::MissingReference`] when the organization does not exist.
pub async fn record(
    conn: &mut PgConnection,
    key: &IdempotencyKey,
    resource_id: Uuid,
) -> Result<RecordOutcome, PersistenceError> {
    let inserted = sqlx::query(
        "INSERT INTO idempotency_keys (organization_id, key, operation, request_hash, resource_id)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (organization_id, key) DO NOTHING",
    )
    .bind(key.organization_id.as_uuid())
    .bind(&key.key)
    .bind(&key.operation)
    .bind(&key.request_hash)
    .bind(resource_id)
    .execute(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write("idempotency key", error))?
    .rows_affected();
    if inserted == 1 {
        return Ok(RecordOutcome::Recorded);
    }
    let row = lookup(conn, key)
        .await?
        .ok_or_else(|| PersistenceError::NotFound {
            entity: "idempotency key",
            id: key.key.clone(),
        })?;
    Ok(RecordOutcome::Replay {
        resource_id: replayed(key, &row)?,
    })
}
