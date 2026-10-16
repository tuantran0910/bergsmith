//! Organizations, principals, and catalog connections.

use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, PrincipalId};
use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::PersistenceError;

/// An organization: the scope for principals, connections, tables, jobs, and
/// idempotency keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Organization {
    /// Identifier.
    pub id: OrganizationId,
    /// Unique, URL-safe name: lowercase letters, digits, and hyphens.
    pub slug: String,
    /// Human-readable name.
    pub display_name: String,
    /// When the organization was created (database time).
    pub created_at: DateTime<Utc>,
}

/// Fields for a new organization.
#[derive(Clone, Debug)]
pub struct NewOrganization {
    /// Identifier to assign.
    pub id: OrganizationId,
    /// Unique, URL-safe name: lowercase letters, digits, and hyphens.
    pub slug: String,
    /// Human-readable name, 1 to 200 characters.
    pub display_name: String,
}

/// Whether a principal is a person or a service account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrincipalKind {
    /// A person who signs in through OIDC.
    User,
    /// A service account, such as a worker pool identity.
    Service,
}

impl PrincipalKind {
    /// Returns the persisted name of the kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Service => "Service",
        }
    }

    fn parse(value: &str) -> Result<Self, PersistenceError> {
        match value {
            "User" => Ok(Self::User),
            "Service" => Ok(Self::Service),
            other => Err(PersistenceError::CorruptValue {
                entity: "principal kind",
                value: other.to_owned(),
            }),
        }
    }
}

/// A person or service account within one organization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    /// Identifier.
    pub id: PrincipalId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Person or service account.
    pub kind: PrincipalKind,
    /// Token issuer, such as an OIDC issuer URL.
    pub issuer: String,
    /// Subject within the issuer. Unique per organization and issuer.
    pub subject: String,
    /// Human-readable name.
    pub display_name: String,
    /// When the principal was created (database time).
    pub created_at: DateTime<Utc>,
}

/// Fields for a new principal.
#[derive(Clone, Debug)]
pub struct NewPrincipal {
    /// Identifier to assign.
    pub id: PrincipalId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Person or service account.
    pub kind: PrincipalKind,
    /// Token issuer.
    pub issuer: String,
    /// Subject within the issuer.
    pub subject: String,
    /// Human-readable name, 1 to 200 characters.
    pub display_name: String,
}

/// A connection to an Iceberg REST catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogConnection {
    /// Identifier.
    pub id: CatalogConnectionId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Unique name within the organization.
    pub name: String,
    /// Catalog base URI (`http` or `https`).
    pub uri: String,
    /// Warehouse to request, if the catalog needs one.
    pub warehouse: Option<String>,
    /// Name of the externally held catalog credential. The secret itself is
    /// never stored in PostgreSQL.
    pub credential_ref: Option<String>,
    /// When the connection was created (database time).
    pub created_at: DateTime<Utc>,
}

/// Fields for a new catalog connection.
#[derive(Clone, Debug)]
pub struct NewCatalogConnection {
    /// Identifier to assign.
    pub id: CatalogConnectionId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// Unique name within the organization: lowercase letters, digits, `_`, `-`.
    pub name: String,
    /// Catalog base URI (`http` or `https`).
    pub uri: String,
    /// Warehouse to request, if the catalog needs one.
    pub warehouse: Option<String>,
    /// Name of the externally held catalog credential.
    pub credential_ref: Option<String>,
}

#[derive(sqlx::FromRow)]
struct OrganizationRow {
    id: Uuid,
    slug: String,
    display_name: String,
    created_at: DateTime<Utc>,
}

impl From<OrganizationRow> for Organization {
    fn from(row: OrganizationRow) -> Self {
        Self {
            id: OrganizationId::from_uuid(row.id),
            slug: row.slug,
            display_name: row.display_name,
            created_at: row.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct PrincipalRow {
    id: Uuid,
    organization_id: Uuid,
    kind: String,
    issuer: String,
    subject: String,
    display_name: String,
    created_at: DateTime<Utc>,
}

impl TryFrom<PrincipalRow> for Principal {
    type Error = PersistenceError;

    fn try_from(row: PrincipalRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: PrincipalId::from_uuid(row.id),
            organization_id: OrganizationId::from_uuid(row.organization_id),
            kind: PrincipalKind::parse(&row.kind)?,
            issuer: row.issuer,
            subject: row.subject,
            display_name: row.display_name,
            created_at: row.created_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct CatalogConnectionRow {
    id: Uuid,
    organization_id: Uuid,
    name: String,
    uri: String,
    warehouse: Option<String>,
    credential_ref: Option<String>,
    created_at: DateTime<Utc>,
}

impl From<CatalogConnectionRow> for CatalogConnection {
    fn from(row: CatalogConnectionRow) -> Self {
        Self {
            id: CatalogConnectionId::from_uuid(row.id),
            organization_id: OrganizationId::from_uuid(row.organization_id),
            name: row.name,
            uri: row.uri,
            warehouse: row.warehouse,
            credential_ref: row.credential_ref,
            created_at: row.created_at,
        }
    }
}

/// Creates an organization.
///
/// # Errors
///
/// [`PersistenceError::AlreadyExists`] when the ID or slug is taken, and
/// [`PersistenceError::InvalidValue`] for a malformed slug or display name.
pub async fn create_organization(
    conn: &mut PgConnection,
    new: &NewOrganization,
) -> Result<Organization, PersistenceError> {
    sqlx::query_as::<_, OrganizationRow>(
        "INSERT INTO organizations (id, slug, display_name) VALUES ($1, $2, $3)
         RETURNING id, slug, display_name, created_at",
    )
    .bind(new.id.as_uuid())
    .bind(&new.slug)
    .bind(&new.display_name)
    .fetch_one(conn)
    .await
    .map(Organization::from)
    .map_err(|error| PersistenceError::from_write("organization", error))
}

/// Looks up an organization by slug.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when no organization has the slug.
pub async fn organization_by_slug(
    conn: &mut PgConnection,
    slug: &str,
) -> Result<Organization, PersistenceError> {
    sqlx::query_as::<_, OrganizationRow>(
        "SELECT id, slug, display_name, created_at FROM organizations WHERE slug = $1",
    )
    .bind(slug)
    .fetch_optional(conn)
    .await?
    .map(Organization::from)
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "organization",
        id: slug.to_owned(),
    })
}

/// Creates a principal in an organization.
///
/// # Errors
///
/// [`PersistenceError::AlreadyExists`] when the ID or the issuer and subject
/// pair is taken in the organization, [`PersistenceError::MissingReference`]
/// when the organization does not exist, and
/// [`PersistenceError::InvalidValue`] for empty fields.
pub async fn create_principal(
    conn: &mut PgConnection,
    new: &NewPrincipal,
) -> Result<Principal, PersistenceError> {
    let row = sqlx::query_as::<_, PrincipalRow>(
        "INSERT INTO principals (id, organization_id, kind, issuer, subject, display_name)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, organization_id, kind, issuer, subject, display_name, created_at",
    )
    .bind(new.id.as_uuid())
    .bind(new.organization_id.as_uuid())
    .bind(new.kind.as_str())
    .bind(&new.issuer)
    .bind(&new.subject)
    .bind(&new.display_name)
    .fetch_one(conn)
    .await
    .map_err(|error| PersistenceError::from_write("principal", error))?;
    row.try_into()
}

/// Looks up the principal an authenticated token belongs to.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the organization has no principal for
/// the issuer and subject.
pub async fn principal_by_subject(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    issuer: &str,
    subject: &str,
) -> Result<Principal, PersistenceError> {
    sqlx::query_as::<_, PrincipalRow>(
        "SELECT id, organization_id, kind, issuer, subject, display_name, created_at
         FROM principals WHERE organization_id = $1 AND issuer = $2 AND subject = $3",
    )
    .bind(organization_id.as_uuid())
    .bind(issuer)
    .bind(subject)
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "principal",
        id: format!("{issuer} {subject}"),
    })?
    .try_into()
}

/// Creates a catalog connection.
///
/// # Errors
///
/// [`PersistenceError::AlreadyExists`] when the ID or name is taken in the
/// organization, [`PersistenceError::MissingReference`] when the organization
/// does not exist, and [`PersistenceError::InvalidValue`] for a malformed name
/// or a URI that is not `http` or `https`.
pub async fn create_catalog_connection(
    conn: &mut PgConnection,
    new: &NewCatalogConnection,
) -> Result<CatalogConnection, PersistenceError> {
    sqlx::query_as::<_, CatalogConnectionRow>(
        "INSERT INTO catalog_connections
             (id, organization_id, name, uri, warehouse, credential_ref)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, organization_id, name, uri, warehouse, credential_ref, created_at",
    )
    .bind(new.id.as_uuid())
    .bind(new.organization_id.as_uuid())
    .bind(&new.name)
    .bind(&new.uri)
    .bind(&new.warehouse)
    .bind(&new.credential_ref)
    .fetch_one(conn)
    .await
    .map(CatalogConnection::from)
    .map_err(|error| PersistenceError::from_write("catalog connection", error))
}

/// Looks up a catalog connection in an organization.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the organization has no such connection.
pub async fn catalog_connection(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: CatalogConnectionId,
) -> Result<CatalogConnection, PersistenceError> {
    sqlx::query_as::<_, CatalogConnectionRow>(
        "SELECT id, organization_id, name, uri, warehouse, credential_ref, created_at
         FROM catalog_connections WHERE organization_id = $1 AND id = $2",
    )
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(conn)
    .await?
    .map(CatalogConnection::from)
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "catalog connection",
        id: id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_kind_round_trips_through_its_persisted_name() {
        for kind in [PrincipalKind::User, PrincipalKind::Service] {
            assert_eq!(PrincipalKind::parse(kind.as_str()).unwrap(), kind);
        }
    }

    #[test]
    fn unknown_principal_kind_is_reported_as_corrupt() {
        let error = PrincipalKind::parse("Robot").unwrap_err();
        assert!(matches!(
            error,
            PersistenceError::CorruptValue { value, .. } if value == "Robot"
        ));
    }
}
