//! Inventory: records what the catalog reports about tables.

use bergsmith_catalog::ObservedTable;
use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, TableId};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::tables::{self, TableAlias};
use sqlx::PgConnection;

/// Records a table observed through a catalog connection and returns its
/// canonical identity.
///
/// Aliases are detected by `table-uuid`: every name that resolves to the
/// same Iceberg table gets the same [`TableId`], so jobs and publication
/// barriers on any alias coordinate with each other.
///
/// # Errors
///
/// [`PersistenceError::TableIdentityConflict`] when the name already belongs
/// to a different table (for example after a drop and recreate), which needs
/// operator review, and the other errors of
/// [`tables::resolve_table`].
pub async fn record_observation(
    conn: &mut PgConnection,
    organization: OrganizationId,
    connection: CatalogConnectionId,
    observed: &ObservedTable,
) -> Result<TableId, PersistenceError> {
    let alias = TableAlias {
        connection_id: connection,
        namespace: observed.name.namespace.clone(),
        name: observed.name.name.clone(),
    };
    Ok(
        tables::resolve_table(conn, organization, &alias, observed.table_uuid)
            .await?
            .id,
    )
}
