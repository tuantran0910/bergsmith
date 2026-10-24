//! Scoped role grants: who may do what, and where.
//!
//! A grant is immutable except for revocation. Authorization reads active
//! grants only, so revoking one takes effect at the next admission or
//! publication check without touching worker credentials.

use bergsmith_common::ids::{
    CatalogConnectionId, OrganizationId, PrincipalId, RoleGrantId, TableId,
};
use bergsmith_common::permission::{GrantScope, Permission, ResourceScope, Role};
use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::PersistenceError;

/// A role granted to one principal within one scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleGrant {
    /// Identifier.
    pub id: RoleGrantId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// The principal that holds the role.
    pub principal_id: PrincipalId,
    /// What the principal may do.
    pub role: Role,
    /// Where the role applies.
    pub scope: GrantScope,
    /// Who granted it, if recorded.
    pub granted_by: Option<PrincipalId>,
    /// When it was granted (database time).
    pub granted_at: DateTime<Utc>,
    /// When it was revoked, if it was.
    pub revoked_at: Option<DateTime<Utc>>,
    /// Who revoked it, if recorded.
    pub revoked_by: Option<PrincipalId>,
}

/// Fields for a new grant.
#[derive(Clone, Copy, Debug)]
pub struct NewRoleGrant {
    /// Identifier to assign.
    pub id: RoleGrantId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// The principal receiving the role.
    pub principal_id: PrincipalId,
    /// What the principal may do.
    pub role: Role,
    /// Where the role applies.
    pub scope: GrantScope,
    /// Who is granting it.
    pub granted_by: Option<PrincipalId>,
}

#[derive(sqlx::FromRow)]
struct RoleGrantRow {
    id: Uuid,
    organization_id: Uuid,
    principal_id: Uuid,
    role: String,
    catalog_connection_id: Option<Uuid>,
    table_identity_id: Option<Uuid>,
    granted_by: Option<Uuid>,
    granted_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
    revoked_by: Option<Uuid>,
}

impl TryFrom<RoleGrantRow> for RoleGrant {
    type Error = PersistenceError;

    fn try_from(row: RoleGrantRow) -> Result<Self, Self::Error> {
        let role = Role::from_name(&row.role).ok_or_else(|| PersistenceError::CorruptValue {
            entity: "role",
            value: row.role.clone(),
        })?;
        let scope = GrantScope {
            catalog_connection: row
                .catalog_connection_id
                .map(CatalogConnectionId::from_uuid),
            table: row.table_identity_id.map(TableId::from_uuid),
        };
        if scope.catalog_connection.is_some() && scope.table.is_some() {
            return Err(PersistenceError::CorruptValue {
                entity: "grant scope",
                value: "connection and table".to_owned(),
            });
        }
        Ok(Self {
            id: RoleGrantId::from_uuid(row.id),
            organization_id: OrganizationId::from_uuid(row.organization_id),
            principal_id: PrincipalId::from_uuid(row.principal_id),
            role,
            scope,
            granted_by: row.granted_by.map(PrincipalId::from_uuid),
            granted_at: row.granted_at,
            revoked_at: row.revoked_at,
            revoked_by: row.revoked_by.map(PrincipalId::from_uuid),
        })
    }
}

/// Columns selected for [`RoleGrant`], in [`RoleGrantRow`] order. A macro so
/// queries stay `&'static str` literals.
macro_rules! grant_columns {
    () => {
        "id, organization_id, principal_id, role, catalog_connection_id, \
         table_identity_id, granted_by, granted_at, revoked_at, revoked_by"
    };
}

/// Creates a grant.
///
/// # Errors
///
/// [`PersistenceError::AlreadyExists`] when the same active role and scope
/// already exists, [`PersistenceError::MissingReference`] for a principal,
/// connection, or table outside the organization, and
/// [`PersistenceError::InvalidValue`] for an invalid scope.
pub async fn create_grant(
    conn: &mut PgConnection,
    new: &NewRoleGrant,
) -> Result<RoleGrant, PersistenceError> {
    if new.scope.catalog_connection.is_some() && new.scope.table.is_some() {
        return Err(PersistenceError::InvalidValue {
            entity: "role grant",
            constraint: "grant_scope_is_connection_or_table".to_owned(),
        });
    }
    sqlx::query_as::<_, RoleGrantRow>(concat!(
        "INSERT INTO principal_role_grants
             (id, organization_id, principal_id, role, catalog_connection_id,
              table_identity_id, granted_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING ",
        grant_columns!()
    ))
    .bind(new.id.as_uuid())
    .bind(new.organization_id.as_uuid())
    .bind(new.principal_id.as_uuid())
    .bind(new.role.as_str())
    .bind(
        new.scope
            .catalog_connection
            .as_ref()
            .map(CatalogConnectionId::as_uuid),
    )
    .bind(new.scope.table.as_ref().map(TableId::as_uuid))
    .bind(new.granted_by.as_ref().map(PrincipalId::as_uuid))
    .fetch_one(conn)
    .await
    .map_err(|error| PersistenceError::from_write("role grant", error))?
    .try_into()
}

/// Creates a grant unless an identical active grant already exists, returning
/// the stored row and whether it was created.
///
/// Safe to race: a concurrent caller that loses the insert re-reads the winner.
///
/// # Errors
///
/// [`PersistenceError::MissingReference`] for a principal, connection, or
/// table outside the organization, and [`PersistenceError::InvalidValue`] for
/// an invalid scope.
pub async fn ensure_grant(
    conn: &mut PgConnection,
    new: &NewRoleGrant,
) -> Result<(RoleGrant, bool), PersistenceError> {
    if new.scope.catalog_connection.is_some() && new.scope.table.is_some() {
        return Err(PersistenceError::InvalidValue {
            entity: "role grant",
            constraint: "grant_scope_is_connection_or_table".to_owned(),
        });
    }
    let inserted = sqlx::query_as::<_, RoleGrantRow>(concat!(
        "INSERT INTO principal_role_grants
             (id, organization_id, principal_id, role, catalog_connection_id,
              table_identity_id, granted_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT DO NOTHING
         RETURNING ",
        grant_columns!()
    ))
    .bind(new.id.as_uuid())
    .bind(new.organization_id.as_uuid())
    .bind(new.principal_id.as_uuid())
    .bind(new.role.as_str())
    .bind(
        new.scope
            .catalog_connection
            .as_ref()
            .map(CatalogConnectionId::as_uuid),
    )
    .bind(new.scope.table.as_ref().map(TableId::as_uuid))
    .bind(new.granted_by.as_ref().map(PrincipalId::as_uuid))
    .fetch_optional(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write("role grant", error))?;
    if let Some(row) = inserted {
        return Ok((row.try_into()?, true));
    }
    let existing = active_grants(conn, new.organization_id, new.principal_id)
        .await?
        .into_iter()
        .find(|grant| grant.role == new.role && grant.scope == new.scope)
        .ok_or_else(|| PersistenceError::NotFound {
            entity: "role grant",
            id: new.id.to_string(),
        })?;
    Ok((existing, false))
}

/// Revokes one active grant.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the organization has no such active
/// grant.
pub async fn revoke_grant(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    grant_id: RoleGrantId,
    revoked_by: Option<PrincipalId>,
) -> Result<RoleGrant, PersistenceError> {
    sqlx::query_as::<_, RoleGrantRow>(concat!(
        "UPDATE principal_role_grants
         SET revoked_at = clock_timestamp(), revoked_by = $3
         WHERE organization_id = $1 AND id = $2 AND revoked_at IS NULL
         RETURNING ",
        grant_columns!()
    ))
    .bind(organization_id.as_uuid())
    .bind(grant_id.as_uuid())
    .bind(revoked_by.as_ref().map(PrincipalId::as_uuid))
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "role grant",
        id: grant_id.to_string(),
    })?
    .try_into()
}

/// Revokes every active grant a principal holds.
///
/// Returns how many grants changed.
///
/// # Errors
///
/// Returns a database error if the update fails.
pub async fn revoke_principal_grants(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    revoked_by: Option<PrincipalId>,
) -> Result<u64, PersistenceError> {
    let result = sqlx::query(
        "UPDATE principal_role_grants
         SET revoked_at = clock_timestamp(), revoked_by = $3
         WHERE organization_id = $1 AND principal_id = $2 AND revoked_at IS NULL",
    )
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(revoked_by.as_ref().map(PrincipalId::as_uuid))
    .execute(conn)
    .await?;
    Ok(result.rows_affected())
}

/// Lists a principal's active grants.
///
/// # Errors
///
/// Returns a database error if the query fails.
pub async fn active_grants(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
) -> Result<Vec<RoleGrant>, PersistenceError> {
    let rows = sqlx::query_as::<_, RoleGrantRow>(concat!(
        "SELECT ",
        grant_columns!(),
        " FROM principal_role_grants
         WHERE organization_id = $1 AND principal_id = $2 AND revoked_at IS NULL
         ORDER BY role, catalog_connection_id NULLS FIRST, table_identity_id NULLS FIRST"
    ))
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// Lists every active grant in an organization, by principal, role, and scope.
///
/// # Errors
///
/// Returns a database error if the query fails.
pub async fn organization_grants(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
) -> Result<Vec<RoleGrant>, PersistenceError> {
    let rows = sqlx::query_as::<_, RoleGrantRow>(concat!(
        "SELECT ",
        grant_columns!(),
        " FROM principal_role_grants
         WHERE organization_id = $1 AND revoked_at IS NULL
         ORDER BY principal_id, role, catalog_connection_id NULLS FIRST,
                  table_identity_id NULLS FIRST"
    ))
    .bind(organization_id.as_uuid())
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// Whether `principal` holds `permission` over `resource`.
///
/// # Errors
///
/// Returns a database error if the check cannot run.
pub async fn has_permission(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    permission: Permission,
    resource: ResourceScope,
) -> Result<bool, PersistenceError> {
    // Deactivation keeps the rows and the grants for audit, so the principal
    // itself decides whether any of them still applies. Admission, publication,
    // and every browser command pass through this check.
    if !crate::tenancy::principal_is_active(&mut *conn, organization_id, principal_id).await? {
        return Ok(false);
    }
    let roles: Vec<String> = permission
        .roles()
        .iter()
        .map(|role| role.as_str().to_owned())
        .collect();
    // A grant is scoped to one organization, so the requested resource must
    // belong to it. This check is independent of the grant shape: an
    // organization-wide grant must not authorize a foreign table or
    // connection whose identifier a caller guessed or supplied.
    let (table, connections) = match resource {
        ResourceScope::Organization => (None, Vec::new()),
        ResourceScope::Connection(connection) => {
            let owned: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM catalog_connections
                 WHERE organization_id = $1 AND id = $2)",
            )
            .bind(organization_id.as_uuid())
            .bind(connection.as_uuid())
            .fetch_one(&mut *conn)
            .await?;
            if !owned {
                return Ok(false);
            }
            (None, vec![*connection.as_uuid()])
        }
        ResourceScope::Table(table) => {
            let owned: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM table_identities
                 WHERE organization_id = $1 AND id = $2)",
            )
            .bind(organization_id.as_uuid())
            .bind(table.as_uuid())
            .fetch_one(&mut *conn)
            .await?;
            if !owned {
                return Ok(false);
            }
            let connections: Vec<Uuid> = sqlx::query_scalar(
                "SELECT catalog_connection_id FROM table_aliases
                 WHERE table_identity_id = $1 AND organization_id = $2",
            )
            .bind(table.as_uuid())
            .bind(organization_id.as_uuid())
            .fetch_all(&mut *conn)
            .await?;
            (Some(*table.as_uuid()), connections)
        }
    };
    let allowed: bool = sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM principal_role_grants g
             WHERE g.organization_id = $1 AND g.principal_id = $2
               AND g.revoked_at IS NULL AND g.role = ANY($3::text[])
               AND (
                   (g.catalog_connection_id IS NULL AND g.table_identity_id IS NULL)
                   OR g.table_identity_id = $4
                   OR g.catalog_connection_id = ANY($5::uuid[])
               )
         )",
    )
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(&roles)
    .bind(table)
    .bind(&connections)
    .fetch_one(conn)
    .await?;
    Ok(allowed)
}

/// Requires `permission` over `resource`, returning a typed denial otherwise.
///
/// # Errors
///
/// [`PersistenceError::PermissionDenied`] when no active grant covers it, and
/// database errors.
pub async fn require_permission(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    permission: Permission,
    resource: ResourceScope,
) -> Result<(), PersistenceError> {
    if has_permission(conn, organization_id, principal_id, permission, resource).await? {
        Ok(())
    } else {
        Err(PersistenceError::PermissionDenied {
            principal: principal_id,
            permission: permission.as_str(),
        })
    }
}

/// Requires that a job's requester may still act on the job's table.
///
/// Every job has a requester, so this always evaluates a grant; there is no
/// unattributed bypass. Jobs created before BS-017 were attributed to a
/// per-organization system principal by migration 0012, whose grant is the
/// documented kill switch for legacy work.
///
/// # Errors
///
/// [`PersistenceError::PermissionDenied`] after revocation and database errors.
pub async fn require_job_permission(
    conn: &mut PgConnection,
    job: &crate::jobs::Job,
    permission: Permission,
) -> Result<(), PersistenceError> {
    require_permission(
        conn,
        job.organization_id,
        job.created_by,
        permission,
        ResourceScope::Table(job.table_id),
    )
    .await
}
