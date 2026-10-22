//! Canonical table identities and their catalog aliases.

use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, PrincipalId, TableId};
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::PersistenceError;
use crate::tenancy::CatalogConnection;

/// A catalog name for a table: connection, namespace path, and table name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableAlias {
    /// The catalog connection the name belongs to.
    pub connection_id: CatalogConnectionId,
    /// Namespace levels, outermost first. Must not be empty.
    pub namespace: Vec<String>,
    /// Table name within the namespace.
    pub name: String,
}

/// A canonical table, keyed by its Iceberg `table-uuid`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableIdentity {
    /// Identifier that jobs and publication barriers attach to.
    pub id: TableId,
    /// Owning organization.
    pub organization_id: OrganizationId,
    /// The table's Iceberg `table-uuid` from its metadata.
    pub table_uuid: Uuid,
    /// When the identity was first seen (database time).
    pub created_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct TableIdentityRow {
    id: Uuid,
    organization_id: Uuid,
    table_uuid: Uuid,
    created_at: DateTime<Utc>,
}

impl From<TableIdentityRow> for TableIdentity {
    fn from(row: TableIdentityRow) -> Self {
        Self {
            id: TableId::from_uuid(row.id),
            organization_id: OrganizationId::from_uuid(row.organization_id),
            table_uuid: row.table_uuid,
            created_at: row.created_at,
        }
    }
}

/// Resolves an observed table to its canonical identity, recording the alias.
///
/// The identity is keyed by `table_uuid`, so every alias of the same table
/// resolves to the same [`TableId`]. Resolving is idempotent.
///
/// # Errors
///
/// - [`PersistenceError::TableIdentityConflict`] when the alias already names
///   a table with a different `table-uuid`, for example after a drop and
///   recreate. The alias is left unchanged.
/// - [`PersistenceError::MissingReference`] when the connection does not exist
///   in the organization.
/// - [`PersistenceError::InvalidValue`] for an empty namespace or name.
pub async fn resolve_table(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    alias: &TableAlias,
    table_uuid: Uuid,
) -> Result<TableIdentity, PersistenceError> {
    let mut tx = conn.begin().await?;

    // The no-op update makes RETURNING yield the existing row on conflict.
    let identity: TableIdentity = sqlx::query_as::<_, TableIdentityRow>(
        "INSERT INTO table_identities (id, organization_id, table_uuid) VALUES ($1, $2, $3)
         ON CONFLICT (organization_id, table_uuid)
             DO UPDATE SET table_uuid = EXCLUDED.table_uuid
         RETURNING id, organization_id, table_uuid, created_at",
    )
    .bind(TableId::generate().as_uuid())
    .bind(organization_id.as_uuid())
    .bind(table_uuid)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("table identity", error))?
    .into();

    sqlx::query(
        "INSERT INTO table_aliases
             (catalog_connection_id, namespace, table_name, organization_id, table_identity_id)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (catalog_connection_id, namespace, table_name) DO NOTHING",
    )
    .bind(alias.connection_id.as_uuid())
    .bind(&alias.namespace)
    .bind(&alias.name)
    .bind(organization_id.as_uuid())
    .bind(identity.id.as_uuid())
    .execute(&mut *tx)
    .await
    .map_err(|error| PersistenceError::from_write("table alias", error))?;

    let existing: Uuid = sqlx::query_scalar(
        "SELECT table_identity_id FROM table_aliases
         WHERE catalog_connection_id = $1 AND namespace = $2 AND table_name = $3",
    )
    .bind(alias.connection_id.as_uuid())
    .bind(&alias.namespace)
    .bind(&alias.name)
    .fetch_one(&mut *tx)
    .await?;

    if existing != *identity.id.as_uuid() {
        return Err(PersistenceError::TableIdentityConflict {
            connection: alias.connection_id,
            existing: TableId::from_uuid(existing),
            observed: identity.id,
        });
    }
    tx.commit().await?;
    Ok(identity)
}

/// Looks up the identity an alias currently names, if any.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn table_by_alias(
    conn: &mut PgConnection,
    alias: &TableAlias,
) -> Result<Option<TableIdentity>, PersistenceError> {
    let row = sqlx::query_as::<_, TableIdentityRow>(
        "SELECT t.id, t.organization_id, t.table_uuid, t.created_at
         FROM table_aliases a JOIN table_identities t ON t.id = a.table_identity_id
         WHERE a.catalog_connection_id = $1 AND a.namespace = $2 AND a.table_name = $3",
    )
    .bind(alias.connection_id.as_uuid())
    .bind(&alias.namespace)
    .bind(&alias.name)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(TableIdentity::from))
}

/// The canonical name a table uses through one connection, if it has one.
///
/// Publication recovery uses this because the immutable intent names the
/// connection; a later grant change must not make retained evidence
/// unreachable.
///
/// # Errors
///
/// Returns a database error if the read fails.
pub async fn alias_for_connection(
    conn: &mut PgConnection,
    connection_id: CatalogConnectionId,
    table_id: TableId,
) -> Result<Option<TableAlias>, PersistenceError> {
    let row: Option<(Vec<String>, String)> = sqlx::query_as(
        "SELECT namespace, table_name FROM table_aliases
         WHERE catalog_connection_id = $1 AND table_identity_id = $2
         ORDER BY namespace, table_name LIMIT 1",
    )
    .bind(connection_id.as_uuid())
    .bind(table_id.as_uuid())
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|(namespace, name)| TableAlias {
        connection_id,
        namespace,
        name,
    }))
}

/// One catalog name for a table, with the connection that serves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableAliasConnection {
    /// The catalog name.
    pub alias: TableAlias,
    /// The connection the name belongs to.
    pub connection: CatalogConnection,
}

#[derive(sqlx::FromRow)]
struct TableAliasConnectionRow {
    catalog_connection_id: Uuid,
    namespace: Vec<String>,
    table_name: String,
    connection_id: Uuid,
    organization_id: Uuid,
    connection_name: String,
    uri: String,
    warehouse: Option<String>,
    credential_ref: Option<String>,
    created_at: DateTime<Utc>,
}

impl From<TableAliasConnectionRow> for TableAliasConnection {
    fn from(row: TableAliasConnectionRow) -> Self {
        Self {
            alias: TableAlias {
                connection_id: CatalogConnectionId::from_uuid(row.catalog_connection_id),
                namespace: row.namespace,
                name: row.table_name,
            },
            connection: CatalogConnection {
                id: CatalogConnectionId::from_uuid(row.connection_id),
                organization_id: OrganizationId::from_uuid(row.organization_id),
                name: row.connection_name,
                uri: row.uri,
                warehouse: row.warehouse,
                credential_ref: row.credential_ref,
                created_at: row.created_at,
            },
        }
    }
}

/// Looks up a canonical table identity.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the organization has no such table.
pub async fn table_identity(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    id: TableId,
) -> Result<TableIdentity, PersistenceError> {
    sqlx::query_as::<_, TableIdentityRow>(
        "SELECT id, organization_id, table_uuid, created_at
         FROM table_identities WHERE organization_id = $1 AND id = $2",
    )
    .bind(organization_id.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(conn)
    .await?
    .map(TableIdentity::from)
    .ok_or_else(|| PersistenceError::NotFound {
        entity: "table",
        id: id.to_string(),
    })
}

/// Lists a table's catalog names that `principal_id` may use, canonically
/// ordered.
///
/// The grant predicate mirrors [`crate::grants::has_permission`] for the same
/// roles: an organization grant covers every name, a table grant covers the
/// table, and a connection grant covers only names on that connection. A
/// caller restricted to one connection therefore never reaches another
/// connection's catalog on this table's behalf.
///
/// The order is by connection ID, then namespace, then table name, so a
/// reader that needs one name gets the same one on every call.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn accessible_aliases_with_connections(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    roles: &[&str],
    table_id: TableId,
) -> Result<Vec<TableAliasConnection>, PersistenceError> {
    let roles: Vec<String> = roles.iter().map(|role| (*role).to_owned()).collect();
    let rows: Vec<TableAliasConnectionRow> = sqlx::query_as(
        "SELECT a.catalog_connection_id, a.namespace, a.table_name,
                c.id AS connection_id, c.organization_id, c.name AS connection_name,
                c.uri, c.warehouse, c.credential_ref, c.created_at
         FROM table_aliases a
         JOIN catalog_connections c
           ON c.organization_id = a.organization_id AND c.id = a.catalog_connection_id
         WHERE a.organization_id = $1 AND a.table_identity_id = $2
           AND EXISTS (
               SELECT 1 FROM principal_role_grants g
               WHERE g.organization_id = $1 AND g.principal_id = $3
                 AND g.revoked_at IS NULL AND g.role = ANY($4::text[])
                 AND ((g.catalog_connection_id IS NULL AND g.table_identity_id IS NULL)
                   OR g.table_identity_id = a.table_identity_id
                   OR g.catalog_connection_id = a.catalog_connection_id))
         ORDER BY a.catalog_connection_id, a.namespace, a.table_name",
    )
    .bind(organization_id.as_uuid())
    .bind(table_id.as_uuid())
    .bind(principal_id.as_uuid())
    .bind(&roles)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(TableAliasConnection::from).collect())
}
