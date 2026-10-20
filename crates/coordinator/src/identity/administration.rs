//! Privileged administration: provisioning principals, and granting,
//! revoking, or terminating sessions.
//!
//! Each command first requires the actor to hold `grants:manage` over the
//! scope it changes, then writes the change and its audit record in the same
//! connection. A failed audit record fails the command, so a privileged change
//! is never applied without a record. These are the commands BS-018 exposes;
//! keeping them here means the audited path is the only path.

use bergsmith_common::ids::{OrganizationId, PrincipalId, RoleGrantId};
use bergsmith_common::permission::{GrantScope, Permission, ResourceScope};
use bergsmith_persistence::grants::{self, NewRoleGrant, RoleGrant};
use bergsmith_persistence::tenancy::{self, NewPrincipal, Principal};
use bergsmith_persistence::{PersistenceError, auth};
use sqlx::{Connection, PgConnection};
use thiserror::Error;

use super::audit;
use super::authorization::Authorizer;

/// Why an administrative command failed.
#[derive(Debug, Error)]
pub enum AdministrationError {
    /// The actor holds no active grant that covers the command.
    #[error(transparent)]
    Authorization(#[from] super::authorization::AuthorizationError),
    /// The change conflicts with existing data or names a missing row.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// The change could not be recorded, so it was not applied.
    #[error(transparent)]
    Audit(#[from] audit::AuditError),
}

impl AdministrationError {
    /// A stable reason code for operator logs.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Authorization(error) => error.reason_code(),
            Self::Persistence(_) => "AdministrationLedgerUnavailable",
            Self::Audit(_) => "AdministrationAuditUnavailable",
        }
    }
}

impl From<sqlx::Error> for AdministrationError {
    fn from(error: sqlx::Error) -> Self {
        Self::Persistence(PersistenceError::from(error))
    }
}

/// Provision a principal for a verified issuer and subject.
///
/// # Errors
///
/// [`AdministrationError`] when the actor is not an administrator, the
/// organization does not match, or the issuer and subject already exist.
pub async fn provision_principal(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    new: &NewPrincipal,
) -> Result<Principal, AdministrationError> {
    if new.organization_id != organization_id {
        return Err(PersistenceError::InvalidValue {
            entity: "principal",
            constraint: "provisioning_must_target_the_same_organization".to_owned(),
        }
        .into());
    }
    // Authorization, the mutation, and its audit row commit together; a
    // failed audit rolls the change back.
    let mut tx = conn.begin().await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageGrants,
        ResourceScope::Organization,
    )
    .await?;
    let principal = tenancy::create_principal(&mut tx, new).await?;
    audit::principal_provisioned(
        &mut tx,
        organization_id,
        Some(actor),
        principal.id,
        &principal.issuer,
    )
    .await?;
    tx.commit().await?;
    Ok(principal)
}

/// Grant a role. The actor must be able to manage grants over the same scope.
///
/// # Errors
///
/// [`AdministrationError`] when the actor is not an administrator over the
/// scope, or the grant already exists.
pub async fn grant_role(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    new: &NewRoleGrant,
) -> Result<RoleGrant, AdministrationError> {
    if new.organization_id != organization_id {
        return Err(PersistenceError::InvalidValue {
            entity: "role grant",
            constraint: "grant_must_target_the_same_organization".to_owned(),
        }
        .into());
    }
    let mut tx = conn.begin().await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageGrants,
        scope_of(new.scope),
    )
    .await?;
    let created = grants::create_grant(&mut tx, new).await?;
    audit::grant_changed(
        &mut tx,
        organization_id,
        Some(actor),
        bergsmith_common::audit::AuditAction::GrantCreated,
        created.principal_id,
        created.role.as_str(),
        Some(&created.id.to_string()),
    )
    .await?;
    tx.commit().await?;
    Ok(created)
}

/// Revoke a role grant. The actor must be able to manage grants over its scope.
///
/// # Errors
///
/// [`AdministrationError`] when the actor is not an administrator over the
/// scope, or the grant is missing or already revoked.
pub async fn revoke_role(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    grant_id: RoleGrantId,
) -> Result<RoleGrant, AdministrationError> {
    let mut tx = conn.begin().await?;
    let existing = active_grant(&mut tx, organization_id, grant_id).await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageGrants,
        scope_of(existing.scope),
    )
    .await?;
    let revoked = grants::revoke_grant(&mut tx, organization_id, grant_id, Some(actor)).await?;
    audit::grant_changed(
        &mut tx,
        organization_id,
        Some(actor),
        bergsmith_common::audit::AuditAction::GrantRevoked,
        revoked.principal_id,
        revoked.role.as_str(),
        Some(&revoked.id.to_string()),
    )
    .await?;
    tx.commit().await?;
    Ok(revoked)
}

/// Revoke every active session of a principal, for example after an incident.
///
/// The actor must be able to manage grants over the organization, because
/// terminating sessions removes access broadly.
///
/// # Errors
///
/// [`AdministrationError`] when the actor is not an administrator, and
/// persistence errors otherwise.
pub async fn revoke_sessions(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: PrincipalId,
    principal_id: PrincipalId,
) -> Result<u64, AdministrationError> {
    let mut tx = conn.begin().await?;
    Authorizer::require(
        &mut tx,
        organization_id,
        actor,
        Permission::ManageGrants,
        ResourceScope::Organization,
    )
    .await?;
    let revoked =
        auth::revoke_principal_sessions(&mut tx, organization_id, principal_id, "revoked").await?;
    if revoked > 0 {
        audit::sessions_revoked(&mut tx, organization_id, Some(actor), principal_id, revoked)
            .await?;
    }
    tx.commit().await?;
    Ok(revoked)
}

fn scope_of(scope: GrantScope) -> ResourceScope {
    match (scope.catalog_connection, scope.table) {
        (_, Some(table)) => ResourceScope::Table(table),
        (Some(connection), None) => ResourceScope::Connection(connection),
        (None, None) => ResourceScope::Organization,
    }
}

async fn active_grant(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    grant_id: RoleGrantId,
) -> Result<RoleGrant, AdministrationError> {
    let principal_id: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT principal_id FROM principal_role_grants
         WHERE organization_id = $1 AND id = $2 AND revoked_at IS NULL",
    )
    .bind(organization_id.as_uuid())
    .bind(grant_id.as_uuid())
    .fetch_optional(&mut *conn)
    .await
    .map_err(PersistenceError::from)?;
    let Some(principal_id) = principal_id else {
        return Err(PersistenceError::NotFound {
            entity: "role grant",
            id: grant_id.to_string(),
        }
        .into());
    };
    let all =
        grants::active_grants(conn, organization_id, PrincipalId::from_uuid(principal_id)).await?;
    all.into_iter()
        .find(|grant| grant.id == grant_id)
        .ok_or_else(|| {
            PersistenceError::NotFound {
                entity: "role grant",
                id: grant_id.to_string(),
            }
            .into()
        })
}
