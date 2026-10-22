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

/// Provisions an organization for a new deployment.
///
/// # Errors
///
/// [`PersistenceError::AlreadyExists`] for a duplicate slug or identifier,
/// [`PersistenceError::InvalidValue`] for a malformed slug or name, and
/// database errors.
pub async fn create_organization(
    url: DatabaseUrl,
    slug: &str,
    display_name: &str,
) -> Result<tenancy::Organization, PersistenceError> {
    use bergsmith_common::ids::OrganizationId;
    let database = connect(url).await?;
    let mut conn = database.pool().acquire().await?;
    tenancy::create_organization(
        &mut conn,
        &tenancy::NewOrganization {
            id: OrganizationId::generate(),
            slug: slug.to_owned(),
            display_name: display_name.to_owned(),
        },
    )
    .await
}

/// Registers a catalog connection in an organization.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] for an unknown organization slug,
/// [`PersistenceError::AlreadyExists`] for a duplicate name, and database
/// errors.
pub async fn add_catalog_connection(
    url: DatabaseUrl,
    organization: &str,
    name: &str,
    uri: &str,
    warehouse: Option<String>,
) -> Result<tenancy::CatalogConnection, PersistenceError> {
    use bergsmith_common::ids::CatalogConnectionId;
    let database = connect(url).await?;
    let mut conn = database.pool().acquire().await?;
    let organization = tenancy::organization_by_slug(&mut conn, organization).await?;
    tenancy::create_catalog_connection(
        &mut conn,
        &tenancy::NewCatalogConnection {
            id: CatalogConnectionId::generate(),
            organization_id: organization.id,
            name: name.to_owned(),
            uri: uri.to_owned(),
            warehouse,
            credential_ref: None,
        },
    )
    .await
}

/// Registers one table alias by loading it from its catalog.
///
/// # Errors
///
/// [`PersistenceError`] for an unknown connection or a conflicting alias, and
/// [`CatalogError`](bergsmith_catalog::CatalogError) when the catalog cannot
/// be reached or does not serve the table.
pub async fn register_table(
    url: DatabaseUrl,
    organization: &str,
    connection: bergsmith_common::ids::CatalogConnectionId,
    namespace: Vec<String>,
    table: &str,
) -> Result<bergsmith_persistence::tables::TableIdentity, RegisterTableError> {
    use bergsmith_catalog::{CatalogConnection, TableName};
    let database = connect(url).await?;
    let mut conn = database
        .pool()
        .acquire()
        .await
        .map_err(PersistenceError::from)?;
    let organization = tenancy::organization_by_slug(&mut conn, organization).await?;
    let connection = tenancy::catalog_connection(&mut conn, organization.id, connection).await?;
    if connection.credential_ref.is_some() {
        return Err(RegisterTableError::CredentialReference);
    }
    let uri =
        url::Url::parse(&connection.uri).map_err(|_| RegisterTableError::InvalidConnection)?;
    let mut config = bergsmith_catalog::ConnectionConfig::new(uri);
    config.warehouse = connection.warehouse.clone();
    let catalog = CatalogConnection::connect(&config)
        .await
        .map_err(RegisterTableError::Catalog)?;
    let name = TableName {
        namespace: namespace.clone(),
        name: table.to_owned(),
    };
    let loaded = catalog
        .load_table(&name, false)
        .await
        .map_err(RegisterTableError::Catalog)?;
    bergsmith_persistence::tables::resolve_table(
        &mut conn,
        organization.id,
        &bergsmith_persistence::tables::TableAlias {
            connection_id: connection.id,
            namespace,
            name: table.to_owned(),
        },
        loaded.metadata.uuid(),
    )
    .await
    .map_err(RegisterTableError::Persistence)
}

/// Why table registration failed.
#[derive(Debug, thiserror::Error)]
pub enum RegisterTableError {
    /// The ledger rejected the alias or identity.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// The catalog could not load the table.
    #[error(transparent)]
    Catalog(#[from] bergsmith_catalog::CatalogError),
    /// The stored connection names an external credential.
    #[error("the connection names a credential reference that this runtime does not resolve")]
    CredentialReference,
    /// The stored connection URI is not a URL.
    #[error("the stored connection URI is not a valid URL")]
    InvalidConnection,
}

/// Provisions a service principal for a worker pool or schedule.
///
/// Without `role`, the principal exists for worker authentication only. A
/// schedule identity also needs the role granted on its table.
///
/// # Errors
///
/// [`PersistenceError`] for an unknown organization, a duplicate identity, or
/// a grant conflict.
pub async fn bootstrap_service(
    url: DatabaseUrl,
    organization: &str,
    display_name: &str,
    role: Option<Role>,
    table: Option<bergsmith_common::ids::TableId>,
) -> Result<tenancy::Principal, PersistenceError> {
    use bergsmith_common::ids::{PrincipalId, RoleGrantId};
    use bergsmith_common::permission::GrantScope;
    let database = connect(url).await?;
    let mut conn = database.pool().acquire().await?;
    let organization = tenancy::organization_by_slug(&mut conn, organization).await?;
    let id = PrincipalId::generate();
    let principal = tenancy::create_principal(
        &mut conn,
        &NewPrincipal {
            id,
            organization_id: organization.id,
            kind: PrincipalKind::Service,
            issuer: "bergsmith:service".into(),
            subject: id.to_string(),
            display_name: display_name.to_owned(),
        },
    )
    .await?;
    if let Some(role) = role {
        let scope = table.map_or_else(GrantScope::organization, GrantScope::table);
        grants::create_grant(
            &mut conn,
            &NewRoleGrant {
                id: RoleGrantId::generate(),
                organization_id: organization.id,
                principal_id: principal.id,
                role,
                scope,
                granted_by: None,
            },
        )
        .await?;
    }
    Ok(principal)
}

/// The stored verifier for a worker pool token.
///
/// # Errors
///
/// [`TokenError`](bergsmith_protocol::identity::TokenError) when the file
/// does not contain a well-formed `bsw1` token.
pub fn service_token_verifier(
    token: &str,
) -> Result<String, bergsmith_protocol::identity::TokenError> {
    Ok(
        bergsmith_protocol::identity::ServiceToken::parse(token.trim())?
            .verifier()
            .to_hex(),
    )
}
