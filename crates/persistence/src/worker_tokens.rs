//! Worker pool bearer-token verifiers stored in PostgreSQL (BS-023.T4).
//!
//! A token is issued once and only its domain-separated verifier is stored.
//! Every worker RPC looks the presented token up by its public pool name and
//! compares the verifier in constant time, so revocation and principal
//! deactivation take effect without a coordinator restart.

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{OrganizationId, PrincipalId, WorkerPoolTokenId};
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::workers::Identity;
use crate::{PersistenceError, tenancy};

/// A worker pool token as operators see it. It never contains the bearer
/// token, which exists only in the response that issued it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerPoolToken {
    /// Identifier used to rotate or revoke the token.
    pub id: WorkerPoolTokenId,
    /// Tenant boundary.
    pub organization_id: OrganizationId,
    /// Service principal the worker session is bound to.
    pub principal_id: PrincipalId,
    /// Worker pool the token belongs to.
    pub pool: String,
    /// Optional operator-provided description.
    pub label: Option<String>,
    /// Who issued it, or `None` for a system bootstrap.
    pub created_by: Option<PrincipalId>,
    /// When it was issued (database time).
    pub created_at: DateTime<Utc>,
    /// The token this one replaced, when issued by rotation.
    pub rotated_from: Option<WorkerPoolTokenId>,
    /// Who revoked it, if it is revoked.
    pub revoked_by: Option<PrincipalId>,
    /// When it was revoked, if it is revoked.
    pub revoked_at: Option<DateTime<Utc>>,
}

impl WorkerPoolToken {
    /// Whether the token can still authenticate a worker.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// Fields for a new token verifier.
#[derive(Clone, Debug)]
pub struct NewWorkerPoolToken {
    /// Identifier to assign.
    pub id: WorkerPoolTokenId,
    /// Tenant boundary.
    pub organization_id: OrganizationId,
    /// Service principal the token authenticates.
    pub principal_id: PrincipalId,
    /// Worker pool named in the token.
    pub pool: String,
    /// Optional operator-provided description.
    pub label: Option<String>,
    /// Domain-separated verifier from `ServiceToken::verifier`.
    pub verifier: Sha256Digest,
    /// Who issued it, or `None` for a system bootstrap.
    pub created_by: Option<PrincipalId>,
    /// The token this one replaces, when issued by rotation.
    pub rotated_from: Option<WorkerPoolTokenId>,
}

/// Stores one verifier.
///
/// # Errors
///
/// [`PersistenceError::AlreadyExists`] when the verifier is already stored,
/// [`PersistenceError::MissingReference`] when the principal or predecessor is
/// outside the organization, and database errors.
pub async fn create(
    conn: &mut PgConnection,
    new: &NewWorkerPoolToken,
) -> Result<WorkerPoolToken, PersistenceError> {
    let row = sqlx::query(
        "INSERT INTO worker_pool_tokens
             (id, organization_id, principal_id, pool, verifier, label, created_by, rotated_from)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING id, organization_id, principal_id, pool, label, created_by,
                   created_at, rotated_from, revoked_by, revoked_at",
    )
    .bind(new.id.as_uuid())
    .bind(new.organization_id.as_uuid())
    .bind(new.principal_id.as_uuid())
    .bind(&new.pool)
    .bind(new.verifier.as_bytes().as_slice())
    .bind(&new.label)
    .bind(new.created_by.map(|id| *id.as_uuid()))
    .bind(new.rotated_from.map(|id| *id.as_uuid()))
    .fetch_one(conn)
    .await
    .map_err(|error| PersistenceError::from_write("worker pool token", error))?;
    from_row(&row)
}

/// Lists a service identity's tokens, active and revoked, in identifier order
/// starting after `after`.
///
/// # Errors
///
/// Returns database errors.
pub async fn list(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    after: Option<WorkerPoolTokenId>,
    limit: i64,
) -> Result<Vec<WorkerPoolToken>, PersistenceError> {
    let rows = sqlx::query(
        "SELECT id, organization_id, principal_id, pool, label, created_by,
                created_at, rotated_from, revoked_by, revoked_at
         FROM worker_pool_tokens
         WHERE organization_id = $1 AND principal_id = $2
           AND ($3::uuid IS NULL OR id > $3)
         ORDER BY id LIMIT $4",
    )
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(after.map(|id| *id.as_uuid()))
    .bind(limit)
    .fetch_all(conn)
    .await?;
    rows.iter().map(from_row).collect()
}

/// Loads one token of a service identity, active or revoked.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the organization, principal, and token
/// do not name a row.
pub async fn token(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    id: WorkerPoolTokenId,
) -> Result<WorkerPoolToken, PersistenceError> {
    let row = sqlx::query(
        "SELECT id, organization_id, principal_id, pool, label, created_by,
                created_at, rotated_from, revoked_by, revoked_at
         FROM worker_pool_tokens
         WHERE organization_id = $1 AND principal_id = $2 AND id = $3",
    )
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "worker pool token",
        id: id.to_string(),
    })?;
    from_row(&row)
}

/// Locks and loads an active token of a service identity. The row lock
/// serializes rotation and revocation, so the second command sees the first
/// one's committed state.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when there is no active token with that
/// identifier for the principal.
pub async fn lock_active(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    id: WorkerPoolTokenId,
) -> Result<WorkerPoolToken, PersistenceError> {
    let row = sqlx::query(
        "SELECT id, organization_id, principal_id, pool, label, created_by,
                created_at, rotated_from, revoked_by, revoked_at
         FROM worker_pool_tokens
         WHERE organization_id = $1 AND principal_id = $2 AND id = $3
           AND revoked_at IS NULL
         FOR UPDATE",
    )
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "worker pool token",
        id: id.to_string(),
    })?;
    from_row(&row)
}

/// Revokes an active token. Revocation is logical: the row stays for audit.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the token is missing or already revoked,
/// and database errors.
pub async fn revoke(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    id: WorkerPoolTokenId,
    actor: Option<PrincipalId>,
) -> Result<WorkerPoolToken, PersistenceError> {
    let row = sqlx::query(
        "UPDATE worker_pool_tokens
         SET revoked_at = clock_timestamp(), revoked_by = $4
         WHERE organization_id = $1 AND principal_id = $2 AND id = $3
           AND revoked_at IS NULL
         RETURNING id, organization_id, principal_id, pool, label, created_by,
                   created_at, rotated_from, revoked_by, revoked_at",
    )
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(id.as_uuid())
    .bind(actor.map(|id| *id.as_uuid()))
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "worker pool token",
        id: id.to_string(),
    })?;
    from_row(&row)
}

/// Resolves a presented bearer token to an active service identity.
///
/// The verifier has a unique index, so this is one indexed row lookup rather
/// than a scan of every token in the pool. The returned verifier and pool are
/// still checked here: the digest comparison runs without early exit, and a
/// token whose parsed pool disagrees with its stored row authenticates
/// nothing. A revoked token or a deactivated principal therefore authenticates
/// nothing immediately.
///
/// # Errors
///
/// Returns database errors.
pub async fn authenticate(
    conn: &mut PgConnection,
    pool: &str,
    presented: &Sha256Digest,
) -> Result<Option<Identity>, PersistenceError> {
    let row = sqlx::query(
        "SELECT t.organization_id, t.principal_id, t.pool, t.verifier
         FROM worker_pool_tokens t
         JOIN principals p
           ON p.organization_id = t.organization_id AND p.id = t.principal_id
         WHERE t.verifier = $1 AND t.revoked_at IS NULL
           AND p.kind = 'Service' AND p.deactivated_at IS NULL",
    )
    .bind(presented.as_bytes().as_slice())
    .fetch_optional(conn)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let stored: Vec<u8> = row.try_get("verifier")?;
    let stored_pool: String = row.try_get("pool")?;
    if !constant_time_eq(presented.as_bytes(), &stored) || stored_pool != pool {
        return Ok(None);
    }
    Ok(Some(Identity {
        organization: OrganizationId::from_uuid(row.try_get("organization_id")?),
        principal: PrincipalId::from_uuid(row.try_get("principal_id")?),
        pool: stored_pool,
    }))
}

/// Confirms a principal is an active service identity.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] for an unknown principal,
/// [`PersistenceError::InvalidValue`] for a person or a deactivated identity,
/// and database errors.
pub async fn require_active_service(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
) -> Result<(), PersistenceError> {
    let principal = tenancy::principal(conn, organization_id, principal_id).await?;
    if principal.kind != tenancy::PrincipalKind::Service || !principal.is_active() {
        return Err(PersistenceError::InvalidValue {
            entity: "worker pool token",
            constraint: "principal_must_be_an_active_service_identity".to_owned(),
        });
    }
    Ok(())
}

fn from_row(row: &sqlx::postgres::PgRow) -> Result<WorkerPoolToken, PersistenceError> {
    let rotated_from: Option<Uuid> = row.try_get("rotated_from")?;
    Ok(WorkerPoolToken {
        id: WorkerPoolTokenId::from_uuid(row.try_get("id")?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id")?),
        principal_id: PrincipalId::from_uuid(row.try_get("principal_id")?),
        pool: row.try_get("pool")?,
        label: row.try_get("label")?,
        created_by: row
            .try_get::<Option<Uuid>, _>("created_by")?
            .map(PrincipalId::from_uuid),
        created_at: row.try_get("created_at")?,
        rotated_from: rotated_from.map(WorkerPoolTokenId::from_uuid),
        revoked_by: row
            .try_get::<Option<Uuid>, _>("revoked_by")?
            .map(PrincipalId::from_uuid),
        revoked_at: row.try_get("revoked_at")?,
    })
}

/// Digests are not secret, but comparing them without early exit keeps the
/// verifier check independent of the matching row's position.
fn constant_time_eq(expected: &[u8], actual: &[u8]) -> bool {
    if expected.len() != actual.len() {
        return false;
    }
    expected
        .iter()
        .zip(actual)
        .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}
