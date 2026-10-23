//! Organizations, principals, and catalog connections.

use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, PrincipalId, TableId};
use bergsmith_common::settings::ConnectionSettings;
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection};
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
    /// Whether new previews and admissions may use this connection.
    pub enabled: bool,
    /// The operating bounds declared for this connection.
    pub settings: ConnectionSettings,
    /// Optimistic configuration revision.
    pub revision: i64,
    /// When configuration was last changed.
    pub updated_at: DateTime<Utc>,
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
    enabled: bool,
    max_execution_seconds: i64,
    max_unresolved_publication_seconds: i64,
    maintenance_enabled: bool,
    revision: i64,
    updated_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
}

impl TryFrom<CatalogConnectionRow> for CatalogConnection {
    type Error = PersistenceError;

    fn try_from(row: CatalogConnectionRow) -> Result<Self, Self::Error> {
        let execution = u64::try_from(row.max_execution_seconds).map_err(|_| {
            PersistenceError::InvalidValue {
                entity: "catalog connection",
                constraint: "the execution budget must be a positive number of seconds".into(),
            }
        })?;
        let unresolved = u64::try_from(row.max_unresolved_publication_seconds).map_err(|_| {
            PersistenceError::InvalidValue {
                entity: "catalog connection",
                constraint: "the unresolved-publication age must be a positive number of seconds"
                    .into(),
            }
        })?;
        let settings = ConnectionSettings::new(execution, unresolved, row.maintenance_enabled)
            .map_err(|error| PersistenceError::InvalidValue {
                entity: "catalog connection",
                constraint: error.to_string(),
            })?;
        Ok(Self {
            id: CatalogConnectionId::from_uuid(row.id),
            organization_id: OrganizationId::from_uuid(row.organization_id),
            name: row.name,
            uri: row.uri,
            warehouse: row.warehouse,
            credential_ref: row.credential_ref,
            enabled: row.enabled,
            settings,
            revision: row.revision,
            updated_at: row.updated_at,
            created_at: row.created_at,
        })
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

/// Looks up a principal by identifier.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the organization has no such principal.
pub async fn principal(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
) -> Result<Principal, PersistenceError> {
    sqlx::query_as::<_, PrincipalRow>(
        "SELECT id, organization_id, kind, issuer, subject, display_name, created_at
         FROM principals WHERE organization_id = $1 AND id = $2",
    )
    .bind(organization_id.as_uuid())
    .bind(principal_id.as_uuid())
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "principal",
        id: principal_id.to_string(),
    })?
    .try_into()
}

/// Creates a principal unless the organization already has that issuer and
/// subject, returning the stored row and whether it was created.
///
/// Safe to race: a concurrent caller that loses the insert re-reads the winner.
///
/// # Errors
///
/// [`PersistenceError::MissingReference`] when the organization does not
/// exist, and [`PersistenceError::InvalidValue`] for empty fields.
pub async fn ensure_principal(
    conn: &mut PgConnection,
    new: &NewPrincipal,
) -> Result<(Principal, bool), PersistenceError> {
    let inserted = sqlx::query_as::<_, PrincipalRow>(
        "INSERT INTO principals (id, organization_id, kind, issuer, subject, display_name)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT DO NOTHING
         RETURNING id, organization_id, kind, issuer, subject, display_name, created_at",
    )
    .bind(new.id.as_uuid())
    .bind(new.organization_id.as_uuid())
    .bind(new.kind.as_str())
    .bind(&new.issuer)
    .bind(&new.subject)
    .bind(&new.display_name)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write("principal", error))?;
    match inserted {
        Some(row) => Ok((row.try_into()?, true)),
        None => Ok((
            principal_by_subject(conn, new.organization_id, &new.issuer, &new.subject).await?,
            false,
        )),
    }
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
    let row = sqlx::query_as::<_, CatalogConnectionRow>(
        "INSERT INTO catalog_connections
             (id, organization_id, name, uri, warehouse, credential_ref)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, organization_id, name, uri, warehouse, credential_ref, enabled,
                   max_execution_seconds, max_unresolved_publication_seconds, maintenance_enabled,
                   revision, updated_at, created_at",
    )
    .bind(new.id.as_uuid())
    .bind(new.organization_id.as_uuid())
    .bind(&new.name)
    .bind(&new.uri)
    .bind(&new.warehouse)
    .bind(&new.credential_ref)
    .fetch_one(conn)
    .await
    .map_err(|error| PersistenceError::from_write("catalog connection", error))?;
    row.try_into()
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
    let row = sqlx::query_as::<_, CatalogConnectionRow>(
        "SELECT id, organization_id, name, uri, warehouse, credential_ref, enabled,
                max_execution_seconds, max_unresolved_publication_seconds, maintenance_enabled,
                revision, updated_at, created_at
         FROM catalog_connections WHERE organization_id = $1 AND id = $2",
    )
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "catalog connection",
        id: id.to_string(),
    })?;
    row.try_into()
}

/// Looks up a catalog connection under a shared row lock.
///
/// The lock keeps a configuration edit from landing while the caller reads
/// the settings it is about to use, so a comparison against those settings
/// is not decided by a race.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the organization has no such connection.
pub async fn catalog_connection_for_share(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: CatalogConnectionId,
) -> Result<CatalogConnection, PersistenceError> {
    let row = sqlx::query_as::<_, CatalogConnectionRow>(
        "SELECT id, organization_id, name, uri, warehouse, credential_ref, enabled,
                max_execution_seconds, max_unresolved_publication_seconds, maintenance_enabled,
                revision, updated_at, created_at
         FROM catalog_connections WHERE organization_id = $1 AND id = $2 FOR SHARE",
    )
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(conn)
    .await?
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "catalog connection",
        id: id.to_string(),
    })?;
    row.try_into()
}

/// Updates connection settings under a row lock and an expected revision.
///
/// Routing and credential references remain unchanged while a nonterminal job
/// may need this connection, including unknown publication outcomes. Disabling
/// is always allowed and never deletes aliases or publication evidence.
///
/// # Errors
///
/// Returns missing-row, revision-conflict, in-use, uniqueness, validation, or
/// database errors. Callers authorize and audit in the enclosing transaction.
pub async fn update_catalog_connection(
    conn: &mut PgConnection,
    new: &NewCatalogConnection,
    expected_revision: i64,
    enabled: bool,
    settings: ConnectionSettings,
) -> Result<CatalogConnection, PersistenceError> {
    let mut tx = conn.begin().await?;
    sqlx::query("SELECT id FROM catalog_connections WHERE organization_id=$1 AND id=$2 FOR UPDATE")
        .bind(new.organization_id.as_uuid())
        .bind(new.id.as_uuid())
        .execute(&mut *tx)
        .await?;
    let current = catalog_connection(&mut tx, new.organization_id, new.id).await?;
    if current.revision != expected_revision {
        return Err(PersistenceError::ConfigRevisionConflict {
            entity: "catalog connection",
            id: new.id.to_string(),
            expected: expected_revision,
            actual: current.revision,
        });
    }
    if current.uri != new.uri
        || current.warehouse != new.warehouse
        || current.credential_ref != new.credential_ref
    {
        let busy: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM jobs j JOIN table_aliases a
             ON a.organization_id=j.organization_id AND a.table_identity_id=j.table_identity_id
             WHERE a.organization_id=$1 AND a.catalog_connection_id=$2
             AND j.state NOT IN ('Succeeded','NoOp','Cancelled','ReplanRequired','Failed'))",
        )
        .bind(new.organization_id.as_uuid())
        .bind(new.id.as_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if busy {
            return Err(PersistenceError::CatalogConnectionInUse { connection: new.id });
        }
    }
    let execution = i64::try_from(settings.max_execution_seconds()).map_err(|_| {
        PersistenceError::InvalidValue {
            entity: "catalog connection",
            constraint: "the execution budget is too large".into(),
        }
    })?;
    let unresolved =
        i64::try_from(settings.max_unresolved_publication_seconds()).map_err(|_| {
            PersistenceError::InvalidValue {
                entity: "catalog connection",
                constraint: "the unresolved-publication age is too large".into(),
            }
        })?;
    sqlx::query(
        "UPDATE catalog_connections SET name=$3, uri=$4, warehouse=$5, credential_ref=$6,
         enabled=$7, max_execution_seconds=$8, max_unresolved_publication_seconds=$9,
         maintenance_enabled=$10, revision=revision+1, updated_at=clock_timestamp()
         WHERE organization_id=$1 AND id=$2",
    )
    .bind(new.organization_id.as_uuid())
    .bind(new.id.as_uuid())
    .bind(&new.name)
    .bind(&new.uri)
    .bind(&new.warehouse)
    .bind(&new.credential_ref)
    .bind(enabled)
    .bind(execution)
    .bind(unresolved)
    .bind(settings.maintenance_enabled())
    .execute(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("catalog connection", error))?;
    let updated = catalog_connection(&mut tx, new.organization_id, new.id).await?;
    tx.commit().await?;
    Ok(updated)
}

/// Locks all connections naming a table in ID order until the caller commits.
/// This serializes job creation and admission with configuration edits without
/// holding a database lock across network I/O.
pub(crate) async fn lock_table_connections(
    conn: &mut PgConnection,
    organization: OrganizationId,
    table: TableId,
) -> Result<Vec<(Uuid, bool, bool)>, PersistenceError> {
    Ok(sqlx::query_as(
        "SELECT c.id, c.enabled, c.maintenance_enabled FROM catalog_connections c
         WHERE c.organization_id=$1 AND EXISTS(SELECT 1 FROM table_aliases a
             WHERE a.organization_id=$1 AND a.table_identity_id=$2
               AND a.catalog_connection_id=c.id)
         ORDER BY c.id FOR SHARE OF c",
    )
    .bind(organization.as_uuid())
    .bind(table.as_uuid())
    .fetch_all(conn)
    .await?)
}

/// The unresolved publications of one connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnresolvedPublications {
    /// How many publications of this connection are unresolved.
    pub count: i64,
    /// The age of the oldest unresolved publication, in seconds.
    pub oldest_seconds: u64,
}

impl UnresolvedPublications {
    /// Whether the oldest unresolved publication is older than `limit_seconds`.
    ///
    /// This is the operator's cue to investigate: the bound never resolves a
    /// publication and never stops a job.
    #[must_use]
    pub const fn is_overdue(self, limit_seconds: u64) -> bool {
        self.count > 0 && self.oldest_seconds > limit_seconds
    }
}

/// Counts the unresolved publications sent through one connection.
///
/// A publication is unresolved while its outcome is unknown: the job is
/// `Committing` with an authorized request, or its outcome is unknown. The
/// publication intent names the connection, so a publication is counted once,
/// for the connection that sent it.
///
/// # Errors
///
/// Returns a database error when the publications cannot be read.
pub async fn unresolved_publications(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: CatalogConnectionId,
) -> Result<UnresolvedPublications, PersistenceError> {
    let (count, oldest): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE j.state = 'CommitOutcomeUnknown'
                    OR (j.state = 'Committing' AND j.publication_started_at IS NOT NULL))::bigint,
                COALESCE(floor(EXTRACT(EPOCH FROM (now() -
                    (min(j.publication_started_at) FILTER (WHERE j.state IN ('Committing', 'CommitOutcomeUnknown'))))))::bigint, 0)
         FROM jobs j
         JOIN publication_intents i ON i.job_id = j.id
         WHERE j.organization_id = $1 AND j.state IN ('Committing', 'CommitOutcomeUnknown')
           AND (i.content->>'catalog_connection_id')::uuid = $2",
    )
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_one(conn)
    .await?;
    Ok(UnresolvedPublications {
        count,
        // A negative age cannot be stored, but the ledger must not fail on one.
        oldest_seconds: u64::try_from(oldest.max(0)).unwrap_or_default(),
    })
}

/// Whether a principal may run maintenance on a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionRoute {
    /// A usable route exists.
    Allowed,
    /// No enabled connection serves the table.
    ConnectionDisabled,
    /// Every enabled connection has maintenance paused.
    MaintenancePaused,
}

impl ExecutionRoute {
    /// Whether maintenance may run on the table.
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// Whether a principal can submit through an enabled alias, locking connection
/// configuration through the caller's transaction. Without an enclosing
/// transaction the locks end after the connection query, so the separate grant
/// query is only a hint; admission must recheck inside its transaction.
///
/// A connection whose maintenance is paused is still enabled: previews and
/// reads work, but no new maintenance runs on its tables. Existing work and
/// the recovery of an unresolved publication are never stopped by the pause.
///
/// The permission and the maintenance eligibility must hold on the same
/// connection: a requester who may only use a paused connection has no usable
/// route, even when another connection serves the table.
///
/// # Errors
///
/// Returns a database error when configuration or grants cannot be read.
pub(crate) async fn lock_execution_route(
    conn: &mut PgConnection,
    organization: OrganizationId,
    principal: PrincipalId,
    table: TableId,
) -> Result<ExecutionRoute, PersistenceError> {
    let connections = lock_table_connections(conn, organization, table).await?;
    let enabled: Vec<Uuid> = connections
        .iter()
        .filter(|(_, enabled, _)| *enabled)
        .map(|(id, _, _)| *id)
        .collect();
    if enabled.is_empty() {
        return Ok(ExecutionRoute::ConnectionDisabled);
    }
    // Only the connections that allow maintenance are usable routes, so the
    // grant is checked against exactly those.
    let usable: Vec<Uuid> = connections
        .iter()
        .filter(|(_, enabled, maintenance)| *enabled && *maintenance)
        .map(|(id, _, _)| *id)
        .collect();
    if usable.is_empty() {
        return Ok(ExecutionRoute::MaintenancePaused);
    }
    let roles: Vec<&str> = bergsmith_common::permission::Permission::SubmitJobs
        .roles()
        .iter()
        .map(|role| role.as_str())
        .collect();
    let granted: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM principal_role_grants g
         WHERE g.organization_id=$1 AND g.principal_id=$2 AND g.revoked_at IS NULL
           AND g.role = ANY($4::text[]) AND (
             (g.catalog_connection_id IS NULL AND g.table_identity_id IS NULL)
             OR g.table_identity_id=$3 OR g.catalog_connection_id=ANY($5::uuid[])))",
    )
    .bind(organization.as_uuid())
    .bind(principal.as_uuid())
    .bind(table.as_uuid())
    .bind(roles)
    .bind(usable)
    .fetch_one(conn)
    .await?;
    // A requester without a grant has no route either. The caller keeps its
    // existing handling for that case.
    Ok(if granted {
        ExecutionRoute::Allowed
    } else {
        ExecutionRoute::ConnectionDisabled
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
