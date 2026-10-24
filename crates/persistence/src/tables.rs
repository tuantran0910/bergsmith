//! Canonical table identities and their catalog aliases.

use std::collections::BTreeMap;

use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, PrincipalId, TableId};
use bergsmith_common::settings::ConnectionSettings;
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
    Ok(register_alias(conn, organization_id, alias, table_uuid)
        .await?
        .identity)
}

/// A registered alias and whether this call recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredAlias {
    /// The canonical table the alias names.
    pub identity: TableIdentity,
    /// Whether this call inserted the alias, rather than finding it registered
    /// already.
    pub created: bool,
}

/// Registers one alias, reporting whether it is new.
///
/// Registering the same name twice is idempotent: the second call answers with
/// `created: false` and the same identity.
///
/// # Errors
///
/// [`resolve_table`]'s errors.
pub async fn register_alias(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    alias: &TableAlias,
    table_uuid: Uuid,
) -> Result<RegisteredAlias, PersistenceError> {
    let mut tx = conn.begin().await?;
    let registered = register_alias_in(&mut tx, organization_id, alias, table_uuid).await?;
    tx.commit().await?;
    Ok(registered)
}

/// Registers aliases in one transaction: either every alias is recorded or
/// none is.
///
/// A namespace registration loads each table from the catalog before this
/// call, so one table an operator may not register leaves the others
/// untouched instead of half-registering the namespace.
///
/// # Errors
///
/// [`resolve_table`]'s errors, for any alias.
pub async fn register_aliases(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    aliases: &[(TableAlias, Uuid)],
) -> Result<Vec<RegisteredAlias>, PersistenceError> {
    let mut tx = conn.begin().await?;
    let registered = register_aliases_in(&mut tx, organization_id, aliases).await?;
    tx.commit().await?;
    Ok(registered)
}

/// Registers aliases on a connection the caller already holds a transaction
/// on, so the caller can record the aliases and their audit together.
///
/// # Errors
///
/// [`resolve_table`]'s errors, for any alias.
pub async fn register_aliases_in(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    aliases: &[(TableAlias, Uuid)],
) -> Result<Vec<RegisteredAlias>, PersistenceError> {
    let mut registered = Vec::with_capacity(aliases.len());
    for (alias, table_uuid) in aliases {
        registered.push(register_alias_in(conn, organization_id, alias, *table_uuid).await?);
    }
    Ok(registered)
}

/// Registers one alias on a connection that is already inside a transaction.
async fn register_alias_in(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    alias: &TableAlias,
    table_uuid: Uuid,
) -> Result<RegisteredAlias, PersistenceError> {
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
    .fetch_one(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write("table identity", error))?
    .into();

    let created = sqlx::query(
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
    .execute(&mut *conn)
    .await
    .map_err(|error| PersistenceError::from_write("table alias", error))?
    .rows_affected()
        == 1;

    let existing: Uuid = sqlx::query_scalar(
        "SELECT table_identity_id FROM table_aliases
         WHERE catalog_connection_id = $1 AND namespace = $2 AND table_name = $3",
    )
    .bind(alias.connection_id.as_uuid())
    .bind(&alias.namespace)
    .bind(&alias.name)
    .fetch_one(&mut *conn)
    .await?;

    if existing != *identity.id.as_uuid() {
        return Err(PersistenceError::TableIdentityConflict {
            connection: alias.connection_id,
            existing: TableId::from_uuid(existing),
            observed: identity.id,
        });
    }
    Ok(RegisteredAlias { identity, created })
}

/// Removes the alias `expected_revision` observed, leaving the table and its
/// other names.
///
/// The revision is compared under the row lock, so a request that read a name
/// which was since removed and registered again removes nothing: the caller is
/// told the name changed and reads it before deciding again.
///
/// Every alias of a table with unfinished work is kept: publication recovery
/// and planning verify what a job may publish through the connection the
/// immutable intent names, so removing the last name that reaches a table
/// could leave a commit unverifiable. The identity itself is never deleted:
/// jobs and publication evidence still reference it.
///
/// # Errors
///
/// [`PersistenceError::NotFound`] when the connection has no such alias,
/// [`PersistenceError::ConfigRevisionConflict`] when the alias is not the row
/// the caller observed, [`PersistenceError::TableInUse`] when a nonterminal
/// job still needs the table, and database errors.
pub async fn unregister_alias(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    alias: &TableAlias,
    expected_revision: i64,
) -> Result<TableId, PersistenceError> {
    let mut tx = conn.begin().await?;
    // Hold the connection the way job creation does, so a job cannot be
    // admitted against this name between the in-use check and the removal.
    sqlx::query("SELECT id FROM catalog_connections WHERE organization_id=$1 AND id=$2 FOR UPDATE")
        .bind(organization_id.as_uuid())
        .bind(alias.connection_id.as_uuid())
        .execute(&mut *tx)
        .await?;
    // Lock the alias row, so the revision cannot change between the
    // comparison and the removal.
    let row: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT table_identity_id, revision FROM table_aliases
         WHERE organization_id = $1 AND catalog_connection_id = $2
           AND namespace = $3 AND table_name = $4
         FOR UPDATE",
    )
    .bind(organization_id.as_uuid())
    .bind(alias.connection_id.as_uuid())
    .bind(&alias.namespace)
    .bind(&alias.name)
    .fetch_optional(&mut *tx)
    .await?;
    let name = format!("{}.{}", alias.namespace.join("."), alias.name);
    let Some((identity, revision)) = row else {
        return Err(PersistenceError::NotFound {
            entity: "table alias",
            id: name,
        });
    };
    if revision != expected_revision {
        return Err(PersistenceError::ConfigRevisionConflict {
            entity: "table alias",
            id: name,
            expected: expected_revision,
            actual: revision,
        });
    }
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM jobs
         WHERE organization_id = $1 AND table_identity_id = $2
           AND state NOT IN ('Succeeded','NoOp','Cancelled','ReplanRequired','Failed'))",
    )
    .bind(organization_id.as_uuid())
    .bind(identity)
    .fetch_one(&mut *tx)
    .await?;
    if busy {
        return Err(PersistenceError::TableInUse {
            table: TableId::from_uuid(identity),
        });
    }
    sqlx::query(
        "DELETE FROM table_aliases
         WHERE organization_id = $1 AND catalog_connection_id = $2
           AND namespace = $3 AND table_name = $4",
    )
    .bind(organization_id.as_uuid())
    .bind(alias.connection_id.as_uuid())
    .bind(&alias.namespace)
    .bind(&alias.name)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(TableId::from_uuid(identity))
}

/// A registered name as the ledger holds it.
///
/// The revision names this row exactly: a name that was removed and registered
/// again carries a new one, so a delayed removal can tell the two apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisteredName {
    /// The canonical table the name reaches.
    pub table_id: TableId,
    /// The row's revision.
    pub revision: i64,
}

/// The named aliases registered on one connection, with their revisions.
///
/// A name the connection has not registered is absent from the map, so a
/// browse request reports what the catalog holds and what Bergsmith tracks in
/// one answer.
///
/// # Errors
///
/// [`PersistenceError::Database`] when the query fails.
pub async fn registered_aliases(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    connection_id: CatalogConnectionId,
    namespace: &[String],
    names: &[String],
) -> Result<BTreeMap<String, RegisteredName>, PersistenceError> {
    if names.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows: Vec<(String, Uuid, i64)> = sqlx::query_as(
        "SELECT table_name, table_identity_id, revision FROM table_aliases
         WHERE organization_id = $1 AND catalog_connection_id = $2
           AND namespace = $3 AND table_name = ANY($4)",
    )
    .bind(organization_id.as_uuid())
    .bind(connection_id.as_uuid())
    .bind(namespace)
    .bind(names)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, identity, revision)| {
            (
                name,
                RegisteredName {
                    table_id: TableId::from_uuid(identity),
                    revision,
                },
            )
        })
        .collect())
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
    enabled: bool,
    max_execution_seconds: i64,
    max_unresolved_publication_seconds: i64,
    maintenance_enabled: bool,
    revision: i64,
    updated_at: DateTime<Utc>,
    created_at: DateTime<Utc>,
}

impl TryFrom<TableAliasConnectionRow> for TableAliasConnection {
    type Error = PersistenceError;

    fn try_from(row: TableAliasConnectionRow) -> Result<Self, Self::Error> {
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
                enabled: row.enabled,
                settings,
                revision: row.revision,
                updated_at: row.updated_at,
                created_at: row.created_at,
            },
        })
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
                c.uri, c.warehouse, c.credential_ref, c.enabled,
                c.max_execution_seconds, c.max_unresolved_publication_seconds,
                c.maintenance_enabled, c.revision, c.updated_at, c.created_at
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
    rows.into_iter()
        .map(TableAliasConnection::try_from)
        .collect::<Result<Vec<_>, _>>()
}
