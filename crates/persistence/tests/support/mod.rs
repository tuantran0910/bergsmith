//! Fixtures shared by the persistence integration tests.
//!
//! Each test file includes this module with `mod support;`, so helpers unused
//! by one file are not dead code in general.
#![allow(dead_code, reason = "each test binary uses a different subset")]
#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "fixtures fail the test on setup errors"
)]

use bergsmith_common::ids::{CatalogConnectionId, OrganizationId, PrincipalId, TableId};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::tables::{self, TableAlias};
use bergsmith_persistence::tenancy::{
    self, NewCatalogConnection, NewOrganization, NewPrincipal, PrincipalKind,
};
use sqlx::PgConnection;
use uuid::Uuid;

/// An organization with one user, one catalog connection, and one table.
pub(crate) struct Tenant {
    pub(crate) organization: OrganizationId,
    pub(crate) user: PrincipalId,
    pub(crate) connection: CatalogConnectionId,
    pub(crate) table: TableId,
}

/// Creates an organization named `slug`.
pub(crate) async fn organization(conn: &mut PgConnection, slug: &str) -> OrganizationId {
    let id = OrganizationId::generate();
    tenancy::create_organization(
        conn,
        &NewOrganization {
            id,
            slug: slug.to_owned(),
            display_name: slug.to_owned(),
        },
    )
    .await
    .expect("organization is created");
    id
}

/// Creates a user principal with `subject` in `organization`.
pub(crate) async fn user(
    conn: &mut PgConnection,
    organization: OrganizationId,
    subject: &str,
) -> PrincipalId {
    let id = PrincipalId::generate();
    tenancy::create_principal(
        conn,
        &NewPrincipal {
            id,
            organization_id: organization,
            kind: PrincipalKind::User,
            issuer: "https://idp.example".to_owned(),
            subject: subject.to_owned(),
            display_name: subject.to_owned(),
        },
    )
    .await
    .expect("principal is created");
    id
}

/// Creates a catalog connection named `name` in `organization`.
pub(crate) async fn connection(
    conn: &mut PgConnection,
    organization: OrganizationId,
    name: &str,
) -> CatalogConnectionId {
    let id = CatalogConnectionId::generate();
    tenancy::create_catalog_connection(
        conn,
        &NewCatalogConnection {
            id,
            organization_id: organization,
            name: name.to_owned(),
            uri: "https://catalog.example/catalog".to_owned(),
            warehouse: Some("warehouse".to_owned()),
            credential_ref: Some("secret/catalog".to_owned()),
        },
    )
    .await
    .expect("connection is created");
    id
}

/// An alias `db.<name>` on `connection`.
pub(crate) fn alias(connection: CatalogConnectionId, name: &str) -> TableAlias {
    TableAlias {
        connection_id: connection,
        namespace: vec!["db".to_owned()],
        name: name.to_owned(),
    }
}

/// Creates a full tenant with a table identity named `slug`.
pub(crate) async fn tenant(conn: &mut PgConnection, slug: &str) -> Tenant {
    let organization = organization(conn, slug).await;
    let user = user(conn, organization, "alice").await;
    let connection = connection(conn, organization, "main").await;
    let table = tables::resolve_table(
        conn,
        organization,
        &alias(connection, "events"),
        Uuid::now_v7(),
    )
    .await
    .expect("table resolves")
    .id;
    Tenant {
        organization,
        user,
        connection,
        table,
    }
}

/// Returns the constraint a typed write error names.
pub(crate) fn constraint(error: &PersistenceError) -> &str {
    match error {
        PersistenceError::AlreadyExists { constraint, .. }
        | PersistenceError::MissingReference { constraint, .. }
        | PersistenceError::InvalidValue { constraint, .. } => constraint,
        other => panic!("expected a constraint violation, got {other:?}"),
    }
}
