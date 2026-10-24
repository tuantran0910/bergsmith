//! Scoped permissions for people and service identities.
//!
//! A principal holds one or more [`Role`] grants. Each grant covers the whole
//! organization, one catalog connection, or one table. BS-017 separates the
//! role (what may be done) from the scope (where), so an operator can be
//! restricted to one connection without inventing a second role. The decision
//! is recorded in ADR 0032.

use std::fmt;

use crate::ids::{CatalogConnectionId, TableId};

/// What a principal is allowed to do, independent of where.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Permission {
    /// List organizations' tables, catalogs, and inventory summaries.
    ViewInventory,
    /// Read jobs, plans, and receipts.
    ViewJobs,
    /// Read or download task artifacts and attempt outputs.
    ViewArtifacts,
    /// Submit a maintenance job.
    SubmitJobs,
    /// Request cancellation of a job.
    CancelJobs,
    /// Create a fresh-plan retry of a terminal job.
    RetryJobs,
    /// Register, rotate, or remove catalog connections.
    ManageConnections,
    /// Create, edit, disable, or run schedules and policies.
    ManageSchedules,
    /// Register worker pools and rotate worker credentials.
    ManageWorkers,
    /// Grant and revoke roles.
    ManageGrants,
    /// Read the organization's audit log.
    ViewAudit,
}

impl Permission {
    /// Every permission, in a stable order.
    pub const ALL: &'static [Self] = &[
        Self::ViewInventory,
        Self::ViewJobs,
        Self::ViewArtifacts,
        Self::SubmitJobs,
        Self::CancelJobs,
        Self::RetryJobs,
        Self::ManageConnections,
        Self::ManageSchedules,
        Self::ManageWorkers,
        Self::ManageGrants,
        Self::ViewAudit,
    ];

    /// The stable name used in audit records and documentation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ViewInventory => "inventory:read",
            Self::ViewJobs => "jobs:read",
            Self::ViewArtifacts => "artifacts:read",
            Self::SubmitJobs => "jobs:submit",
            Self::CancelJobs => "jobs:cancel",
            Self::RetryJobs => "jobs:retry",
            Self::ManageConnections => "connections:manage",
            Self::ManageSchedules => "schedules:manage",
            Self::ManageWorkers => "workers:manage",
            Self::ManageGrants => "grants:manage",
            Self::ViewAudit => "audit:read",
        }
    }

    /// The roles that carry this permission.
    #[must_use]
    pub const fn roles(self) -> &'static [Role] {
        match self {
            Self::ViewInventory | Self::ViewJobs | Self::ViewArtifacts => &[
                Role::Viewer,
                Role::Operator,
                Role::Administrator,
                Role::MaintenanceService,
            ],
            Self::SubmitJobs | Self::RetryJobs => &[
                Role::Operator,
                Role::Administrator,
                Role::MaintenanceService,
            ],
            Self::CancelJobs => &[Role::Operator, Role::Administrator],
            Self::ManageConnections
            | Self::ManageSchedules
            | Self::ManageWorkers
            | Self::ManageGrants
            | Self::ViewAudit => &[Role::Administrator],
        }
    }
}

/// A named set of permissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Read-only access to inventory, jobs, and artifacts.
    Viewer,
    /// A viewer who may submit, cancel, and retry jobs.
    Operator,
    /// Full organization administration, including grants and audit.
    Administrator,
    /// A service account that runs scheduled maintenance: read access plus
    /// job submission and retry, but no cancellation or administration.
    MaintenanceService,
}

impl Role {
    /// Every role, in a stable order.
    pub const ALL: &'static [Self] = &[
        Self::Viewer,
        Self::Operator,
        Self::Administrator,
        Self::MaintenanceService,
    ];

    /// The stable name persisted in PostgreSQL and shown in audit records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "Viewer",
            Self::Operator => "Operator",
            Self::Administrator => "Administrator",
            Self::MaintenanceService => "MaintenanceService",
        }
    }

    /// Parses the persisted name, or `None` if it is unrecognized.
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|role| role.as_str() == value)
    }

    /// Whether this role carries `permission`.
    #[must_use]
    pub fn allows(self, permission: Permission) -> bool {
        permission.roles().contains(&self)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The part of the organization a grant covers.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(default, rename_all = "snake_case", deny_unknown_fields)]
pub struct GrantScope {
    /// Restricts the grant to one catalog connection.
    pub catalog_connection: Option<CatalogConnectionId>,
    /// Restricts the grant to one canonical table.
    pub table: Option<TableId>,
}

impl GrantScope {
    /// The whole organization.
    #[must_use]
    pub const fn organization() -> Self {
        Self {
            catalog_connection: None,
            table: None,
        }
    }

    /// One catalog connection, including every table behind it.
    #[must_use]
    pub const fn connection(catalog_connection: CatalogConnectionId) -> Self {
        Self {
            catalog_connection: Some(catalog_connection),
            table: None,
        }
    }

    /// One canonical table.
    #[must_use]
    pub const fn table(table: TableId) -> Self {
        Self {
            catalog_connection: None,
            table: Some(table),
        }
    }

    /// Whether the grant covers an organization-wide resource.
    #[must_use]
    pub const fn is_organization(self) -> bool {
        self.catalog_connection.is_none() && self.table.is_none()
    }

    /// The resource this grant applies to.
    ///
    /// A table grant is narrower than a connection grant, which is narrower
    /// than an organization grant, so authorization checks the most specific
    /// resource the grant names.
    #[must_use]
    pub const fn resource(self) -> ResourceScope {
        match (self.catalog_connection, self.table) {
            (_, Some(table)) => ResourceScope::Table(table),
            (Some(connection), None) => ResourceScope::Connection(connection),
            (None, None) => ResourceScope::Organization,
        }
    }

    /// Whether this scope covers a table reached through `connections`.
    ///
    /// An organization grant always covers the table; a table grant covers it
    /// by identity; a connection grant covers it when the table has an alias
    /// on that connection.
    #[must_use]
    pub fn covers_table(self, table: TableId, connections: &[CatalogConnectionId]) -> bool {
        match (self.catalog_connection, self.table) {
            (None, None) => true,
            (None, Some(granted)) => granted == table,
            (Some(connection), None) => connections.contains(&connection),
            (Some(_), Some(_)) => false,
        }
    }

    /// Whether this scope covers a catalog connection itself.
    #[must_use]
    pub fn covers_connection(self, connection: CatalogConnectionId) -> bool {
        match (self.catalog_connection, self.table) {
            (None, None) => true,
            (Some(granted), None) => granted == connection,
            (None | Some(_), Some(_)) => false,
        }
    }
}

/// What an authorization decision protects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceScope {
    /// The whole organization, such as the principal list.
    Organization,
    /// One catalog connection and its tables.
    Connection(CatalogConnectionId),
    /// One canonical table.
    Table(TableId),
}

/// A decision made by an authorization check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The principal holds a grant that covers the requested permission.
    Allowed,
    /// No active grant covers the request.
    Denied,
}

impl Decision {
    /// Whether the action may proceed.
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_permissions_are_stable_supersets() {
        for permission in Permission::ALL {
            assert!(
                Role::Administrator.allows(*permission),
                "administrator misses {}",
                permission.as_str()
            );
        }
        for permission in [
            Permission::ViewInventory,
            Permission::ViewJobs,
            Permission::ViewArtifacts,
            Permission::SubmitJobs,
            Permission::RetryJobs,
        ] {
            assert!(Role::MaintenanceService.allows(permission));
        }
        assert!(!Role::MaintenanceService.allows(Permission::CancelJobs));
        assert!(!Role::Viewer.allows(Permission::SubmitJobs));
        assert!(!Role::Operator.allows(Permission::ManageGrants));
        assert!(!Role::Operator.allows(Permission::ViewAudit));
    }

    #[test]
    fn every_permission_has_at_least_one_role() {
        for permission in Permission::ALL {
            assert!(!permission.roles().is_empty());
        }
    }

    #[test]
    fn organization_grants_cover_every_resource() {
        let scope = GrantScope::organization();
        assert!(scope.covers_table(TableId::generate(), &[]));
        assert!(scope.covers_connection(CatalogConnectionId::generate()));
    }

    #[test]
    fn table_grants_cover_only_their_table() {
        let table = TableId::generate();
        let other = TableId::generate();
        let scope = GrantScope::table(table);
        assert!(scope.covers_table(table, &[]));
        assert!(!scope.covers_table(other, &[]));
        assert!(!scope.covers_connection(CatalogConnectionId::generate()));
    }

    #[test]
    fn connection_grants_cover_aliases_of_their_connection() {
        let connection = CatalogConnectionId::generate();
        let scope = GrantScope::connection(connection);
        assert!(scope.covers_connection(connection));
        assert!(scope.covers_table(TableId::generate(), &[connection]));
        assert!(!scope.covers_table(TableId::generate(), &[CatalogConnectionId::generate()]));
    }
}
