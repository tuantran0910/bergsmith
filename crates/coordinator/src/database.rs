//! Database administration commands run by the coordinator binary.

use std::time::Duration;

use bergsmith_common::audit::AuditAction;
use bergsmith_common::ids::{PrincipalId, RoleGrantId};
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::tenancy::{self, NewPrincipal, PrincipalKind};
use bergsmith_persistence::{Database, DatabaseConfig};
pub use bergsmith_persistence::{DatabaseUrl, PersistenceError};
use sqlx::{Connection, PgConnection};

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
    connect(url).await?.migrate().await
}

/// The first administrator to seed before any user can sign in.
#[derive(Clone, Debug)]
pub struct BootstrapAdministrator {
    /// Organization slug the administrator belongs to.
    pub organization: String,
    /// OIDC issuer the user signs in through.
    pub issuer: String,
    /// OIDC subject within that issuer.
    pub subject: String,
    /// Human-readable name.
    pub display_name: String,
}

/// Provisions the first administrator for an organization.
///
/// Login never creates principals or grants, so this is the documented way to
/// bootstrap a deployment. It is idempotent, including under a concurrent
/// invocation: an existing principal or administrator grant is reused rather
/// than duplicated. The change is audited as a system action.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the organization slug is unknown, and
/// database errors otherwise, including a failed audit record (which fails the
/// command so a provisioned administrator is never unrecorded).
pub async fn bootstrap_administrator(
    conn: &mut PgConnection,
    request: &BootstrapAdministrator,
) -> Result<PrincipalId, PersistenceError> {
    let mut tx = conn.begin().await?;
    let organization = tenancy::organization_by_slug(&mut tx, &request.organization).await?;
    let (principal, created) = tenancy::ensure_principal(
        &mut tx,
        &NewPrincipal {
            id: PrincipalId::generate(),
            organization_id: organization.id,
            kind: PrincipalKind::User,
            issuer: request.issuer.clone(),
            subject: request.subject.clone(),
            display_name: request.display_name.clone(),
        },
    )
    .await?;
    if created {
        crate::identity::audit::principal_provisioned(
            &mut tx,
            organization.id,
            None,
            principal.id,
            &principal.issuer,
        )
        .await?;
    }
    let (grant, granted) = grants::ensure_grant(
        &mut tx,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: organization.id,
            principal_id: principal.id,
            role: Role::Administrator,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await?;
    if granted {
        crate::identity::audit::grant_changed(
            &mut tx,
            organization.id,
            None,
            AuditAction::GrantCreated,
            principal.id,
            grant.role.as_str(),
            Some(&grant.id.to_string()),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(principal.id)
}

/// Connects with the single-connection migration/admin profile.
///
/// # Errors
///
/// Returns [`PersistenceError::Database`] when the database is unreachable.
pub async fn connect(url: DatabaseUrl) -> Result<Database, PersistenceError> {
    let mut config = DatabaseConfig::new(url);
    config.max_connections = 1;
    config.acquire_timeout = MIGRATE_ACQUIRE_TIMEOUT;
    Database::connect(&config).await
}
