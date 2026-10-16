//! Canonical table identities and their catalog aliases.

use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, TableId};
use chrono::{DateTime, Utc};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

use crate::PersistenceError;

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
