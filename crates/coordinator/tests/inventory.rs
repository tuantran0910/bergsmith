//! Alias detection when recording catalog observations (BS-005.T2).

// clippy's `allow-unwrap-in-tests` covers only `#[test]` functions; the
// fixtures below fail the test on setup errors.
#![expect(clippy::unwrap_used, reason = "test fixtures fail the test")]

use bergsmith_catalog::{ObservedTable, TableName};
use bergsmith_common::ids::{CatalogConnectionId, OrganizationId};
use bergsmith_coordinator::inventory::record_observation;
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::tenancy::{self, NewCatalogConnection, NewOrganization};
use bergsmith_storage::Location;
use sqlx::PgPool;
use uuid::Uuid;

async fn tenant(pool: &PgPool) -> (OrganizationId, CatalogConnectionId, CatalogConnectionId) {
    let mut conn = pool.acquire().await.unwrap();
    let organization = OrganizationId::generate();
    tenancy::create_organization(
        &mut conn,
        &NewOrganization {
            id: organization,
            slug: "acme".to_owned(),
            display_name: "Acme".to_owned(),
        },
    )
    .await
    .unwrap();
    let mut connections = Vec::new();
    for name in ["main", "replica"] {
        let id = CatalogConnectionId::generate();
        tenancy::create_catalog_connection(
            &mut conn,
            &NewCatalogConnection {
                id,
                organization_id: organization,
                name: name.to_owned(),
                uri: "http://127.0.0.1:58181/catalog".to_owned(),
                warehouse: Some("demo".to_owned()),
                credential_ref: None,
            },
        )
        .await
        .unwrap();
        connections.push(id);
    }
    (organization, connections[0], connections[1])
}

fn observed(name: &str, table_uuid: Uuid) -> ObservedTable {
    ObservedTable {
        name: TableName {
            namespace: vec!["db".to_owned()],
            name: name.to_owned(),
        },
        table_uuid,
        location: Location::parse("s3://warehouse/demo/t").unwrap(),
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn names_of_one_table_share_a_canonical_identity(pool: PgPool) {
    let (organization, main, replica) = tenant(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let table_uuid = Uuid::now_v7();

    let first = record_observation(
        &mut conn,
        organization,
        main,
        &observed("events", table_uuid),
    )
    .await
    .unwrap();
    let again = record_observation(
        &mut conn,
        organization,
        main,
        &observed("events", table_uuid),
    )
    .await
    .unwrap();
    let alias = record_observation(
        &mut conn,
        organization,
        replica,
        &observed("events_v", table_uuid),
    )
    .await
    .unwrap();

    assert_eq!(first, again);
    assert_eq!(first, alias);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_name_reused_by_a_different_table_is_a_conflict(pool: PgPool) {
    let (organization, main, _) = tenant(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    record_observation(
        &mut conn,
        organization,
        main,
        &observed("events", Uuid::now_v7()),
    )
    .await
    .unwrap();

    let error = record_observation(
        &mut conn,
        organization,
        main,
        &observed("events", Uuid::now_v7()),
    )
    .await
    .unwrap_err();

    assert!(
        matches!(error, PersistenceError::TableIdentityConflict { .. }),
        "{error}"
    );
}
