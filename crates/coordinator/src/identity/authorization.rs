//! Scoped authorization decisions and maintenance service identities.
//!
//! Every command that changes durable state calls [`Authorizer`] with a
//! permission and a resource. Admission, intent construction, and send
//! authorization re-read active grants inside the ledger transaction that is
//! about to act (`grants::require_job_permission`), so revocation cannot race
//! a cached decision; [`Authorizer::require_job`] is the coordinator-facing
//! wrapper over the same check.

use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, PrincipalId, TableId};
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::grants::{self, RoleGrant};
use bergsmith_persistence::jobs::Job;
use bergsmith_persistence::tenancy::{self, PrincipalKind};
use sqlx::PgConnection;
use thiserror::Error;

/// Why an authorization check failed.
#[derive(Debug, Error)]
pub enum AuthorizationError {
    /// The principal holds no active grant covering the request.
    #[error("principal {principal} is not permitted to {permission}")]
    Denied {
        /// The principal that was checked.
        principal: PrincipalId,
        /// Stable permission name.
        permission: &'static str,
    },
    /// A schedule identity must be a service account, not a person.
    #[error("principal {principal} is not a service account")]
    NotServicePrincipal {
        /// The principal that was checked.
        principal: PrincipalId,
    },
    /// The identity store failed.
    #[error(transparent)]
    Persistence(PersistenceError),
}

impl AuthorizationError {
    /// A stable reason code for operator logs.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Denied { .. } => "AuthorizationDenied",
            Self::NotServicePrincipal { .. } => "AuthorizationServicePrincipalRequired",
            Self::Persistence(_) => "AuthorizationLedgerUnavailable",
        }
    }
}

impl From<PersistenceError> for AuthorizationError {
    fn from(error: PersistenceError) -> Self {
        match error {
            PersistenceError::PermissionDenied {
                principal,
                permission,
            } => Self::Denied {
                principal,
                permission,
            },
            other => Self::Persistence(other),
        }
    }
}

/// Stateless authorization checks backed by active role grants.
#[derive(Clone, Copy, Debug)]
pub struct Authorizer;

impl Authorizer {
    /// Lists the principal's active grants, for a session summary or an
    /// administration screen.
    ///
    /// # Errors
    ///
    /// Returns a persistence error.
    pub async fn grants(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        principal_id: PrincipalId,
    ) -> Result<Vec<RoleGrant>, AuthorizationError> {
        Ok(grants::active_grants(conn, organization_id, principal_id).await?)
    }

    /// Requires one permission over one resource.
    ///
    /// # Errors
    ///
    /// [`AuthorizationError::Denied`] and persistence errors.
    pub async fn require(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        principal_id: PrincipalId,
        permission: Permission,
        resource: ResourceScope,
    ) -> Result<(), AuthorizationError> {
        Ok(
            grants::require_permission(conn, organization_id, principal_id, permission, resource)
                .await?,
        )
    }

    /// Rechecks a job requester's grant against the job's table.
    ///
    /// # Errors
    ///
    /// [`AuthorizationError::Denied`] after revocation and persistence errors.
    pub async fn require_job(
        conn: &mut PgConnection,
        job: &Job,
        permission: Permission,
    ) -> Result<(), AuthorizationError> {
        Ok(grants::require_job_permission(conn, job, permission).await?)
    }

    /// Lists the catalog connections a principal may see, in name order.
    ///
    /// A table-scoped grant does not reveal its connection; it only reveals
    /// the table through [`visible_tables`](Self::visible_tables).
    ///
    /// # Errors
    ///
    /// Returns a persistence error.
    pub async fn visible_connections(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        principal_id: PrincipalId,
        permission: Permission,
    ) -> Result<Vec<CatalogConnectionId>, AuthorizationError> {
        let roles = role_names(permission);
        let rows: Vec<uuid::Uuid> = sqlx::query_scalar(
            "SELECT c.id FROM catalog_connections c
             WHERE c.organization_id = $1
               AND (
                   EXISTS (
                       SELECT 1 FROM principal_role_grants g
                       WHERE g.organization_id = $1 AND g.principal_id = $2
                         AND g.revoked_at IS NULL AND g.role = ANY($3::text[])
                         AND g.catalog_connection_id IS NULL
                         AND g.table_identity_id IS NULL)
                   OR EXISTS (
                       SELECT 1 FROM principal_role_grants g
                       WHERE g.organization_id = $1 AND g.principal_id = $2
                         AND g.revoked_at IS NULL AND g.role = ANY($3::text[])
                         AND g.catalog_connection_id = c.id)
               )
             ORDER BY c.name",
        )
        .bind(organization_id.as_uuid())
        .bind(principal_id.as_uuid())
        .bind(&roles)
        .fetch_all(conn)
        .await
        .map_err(PersistenceError::from)?;
        Ok(rows
            .into_iter()
            .map(CatalogConnectionId::from_uuid)
            .collect())
    }

    /// Lists the canonical tables a principal may see.
    ///
    /// This is the filter inventory and search apply, so a principal with a
    /// connection grant cannot scan another connection's tables.
    ///
    /// # Errors
    ///
    /// Returns a persistence error.
    pub async fn visible_tables(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        principal_id: PrincipalId,
        permission: Permission,
    ) -> Result<Vec<TableId>, AuthorizationError> {
        let roles = role_names(permission);
        let rows: Vec<uuid::Uuid> = sqlx::query_scalar(
            "SELECT t.id FROM table_identities t
             WHERE t.organization_id = $1
               AND (
                   EXISTS (
                       SELECT 1 FROM principal_role_grants g
                       WHERE g.organization_id = $1 AND g.principal_id = $2
                         AND g.revoked_at IS NULL AND g.role = ANY($3::text[])
                         AND g.catalog_connection_id IS NULL
                         AND g.table_identity_id IS NULL)
                   OR EXISTS (
                       SELECT 1 FROM principal_role_grants g
                       WHERE g.organization_id = $1 AND g.principal_id = $2
                         AND g.revoked_at IS NULL AND g.role = ANY($3::text[])
                         AND g.table_identity_id = t.id)
                   OR EXISTS (
                       SELECT 1 FROM principal_role_grants g
                       JOIN table_aliases a
                         ON a.organization_id = g.organization_id
                        AND a.table_identity_id = t.id
                       WHERE g.organization_id = $1 AND g.principal_id = $2
                         AND g.revoked_at IS NULL AND g.role = ANY($3::text[])
                         AND g.catalog_connection_id = a.catalog_connection_id)
               )
             ORDER BY t.id",
        )
        .bind(organization_id.as_uuid())
        .bind(principal_id.as_uuid())
        .bind(&roles)
        .fetch_all(conn)
        .await
        .map_err(PersistenceError::from)?;
        Ok(rows.into_iter().map(TableId::from_uuid).collect())
    }
}

fn role_names(permission: Permission) -> Vec<String> {
    permission
        .roles()
        .iter()
        .map(|role| role.as_str().to_owned())
        .collect()
}

/// A schedule's explicit service identity.
///
/// BS-019 creates schedules; this type is the contract they must use. A
/// schedule binds one organization service principal, and every job it creates
/// is attributed to that principal. Revoking the principal's grant stops new
/// admission and publication on the next check, while worker tokens and
/// sessions are untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenanceService {
    organization_id: OrganizationId,
    principal_id: PrincipalId,
}

impl MaintenanceService {
    /// Binds a service principal as a maintenance identity.
    ///
    /// # Errors
    ///
    /// [`AuthorizationError::NotServicePrincipal`] for a person,
    /// [`AuthorizationError::Denied`] when the principal has no active grant
    /// that could submit work, and persistence errors.
    pub async fn bind(
        conn: &mut PgConnection,
        organization_id: OrganizationId,
        principal_id: PrincipalId,
    ) -> Result<Self, AuthorizationError> {
        let principal = tenancy::principal(conn, organization_id, principal_id).await?;
        if principal.kind != PrincipalKind::Service {
            return Err(AuthorizationError::NotServicePrincipal {
                principal: principal_id,
            });
        }
        let grants = grants::active_grants(conn, organization_id, principal_id).await?;
        if !grants
            .iter()
            .any(|grant| grant.role.allows(Permission::SubmitJobs))
        {
            return Err(AuthorizationError::Denied {
                principal: principal_id,
                permission: Permission::SubmitJobs.as_str(),
            });
        }
        Ok(Self {
            organization_id,
            principal_id,
        })
    }

    /// The bound organization.
    #[must_use]
    pub const fn organization_id(self) -> OrganizationId {
        self.organization_id
    }

    /// The bound service principal.
    #[must_use]
    pub const fn principal_id(self) -> PrincipalId {
        self.principal_id
    }

    /// Requires the service identity to still hold `permission` over the
    /// job's table.
    ///
    /// # Errors
    ///
    /// [`AuthorizationError::Denied`] when the grant was revoked or narrowed,
    /// and persistence errors.
    pub async fn authorize_job(
        self,
        conn: &mut PgConnection,
        job: &Job,
        permission: Permission,
    ) -> Result<(), AuthorizationError> {
        if job.organization_id != self.organization_id || job.created_by != self.principal_id {
            return Err(AuthorizationError::Denied {
                principal: self.principal_id,
                permission: permission.as_str(),
            });
        }
        Authorizer::require(
            conn,
            self.organization_id,
            self.principal_id,
            permission,
            ResourceScope::Table(job.table_id),
        )
        .await
    }
}
