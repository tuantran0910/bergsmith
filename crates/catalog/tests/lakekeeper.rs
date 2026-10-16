//! Catalog connections against the Compose Lakekeeper, with RustFS vending
//! STS credentials (BS-005.T1, T2, T3, T6).

mod support;

use std::time::Duration;

use bergsmith_catalog::{CatalogConnection, CatalogError, TableName};
use bergsmith_common::ids::{AttemptId, JobId};
use bergsmith_storage::{CredentialCell, Location, ObjectStore, S3Profile, StorageProfile};
use bytes::Bytes;
use support::{Warehouse, s3_endpoint};

fn rustfs_profile() -> StorageProfile {
    StorageProfile::S3(S3Profile {
        endpoint: Some(s3_endpoint()),
        region: "us-east-1".to_owned(),
        path_style_access: true,
    })
}

#[tokio::test]
async fn connecting_reports_lakekeeper_capabilities() {
    let warehouse = Warehouse::create().await;

    let connection = warehouse.connect().await;
    let capabilities = connection.capabilities();

    assert!(capabilities.credentials_refresh);
    assert_eq!(
        capabilities.idempotency_key_lifetime,
        Some(Duration::from_secs(30 * 60))
    );
    assert!(capabilities.missing_required().is_empty());
}

#[tokio::test]
async fn connecting_to_an_unknown_warehouse_fails() {
    let mut config = Warehouse::create().await.config();
    config.warehouse = Some("no-such-warehouse".to_owned());

    let error = CatalogConnection::connect(&config).await.err().unwrap();

    assert!(matches!(error, CatalogError::Status { .. }), "{error}");
}

#[tokio::test]
async fn tables_are_listed_by_namespace() {
    let warehouse = Warehouse::create().await;
    let events = warehouse.create_table("db", "events").await;
    let audit = warehouse.create_table("db", "audit").await;
    let connection = warehouse.connect().await;

    let tables = connection.list_tables(&["db".to_owned()]).await.unwrap();
    let missing = connection
        .list_tables(&["nope".to_owned()])
        .await
        .unwrap_err();

    assert_eq!(tables, [audit, events]);
    assert!(missing.is_not_found(), "{missing}");
}

#[tokio::test]
async fn loading_vends_credentials_scoped_to_the_table() {
    let warehouse = Warehouse::create().await;
    let name = warehouse.create_table("db", "events").await;
    let connection = warehouse.connect().await;

    let table = connection.load_table(&name, true).await.unwrap();
    let plain = connection.load_table(&name, false).await.unwrap();

    assert!(!table.credentials.is_empty());
    for credential in &table.credentials {
        assert!(table.location.is_within(&credential.prefix));
        assert!(credential.expires_at.is_some());
    }
    assert!(plain.credentials.is_empty());
    assert_eq!(table.observation().table_uuid, plain.metadata.uuid());
    assert!(table.metadata_location.is_some());
}

#[tokio::test]
async fn missing_tables_are_not_found() {
    let warehouse = Warehouse::create().await;
    warehouse.create_table("db", "events").await;
    let connection = warehouse.connect().await;
    let missing = TableName {
        namespace: vec!["db".to_owned()],
        name: "missing".to_owned(),
    };

    let error = connection.load_table(&missing, true).await.unwrap_err();

    assert!(error.is_not_found(), "{error}");
}

#[tokio::test]
async fn vended_credentials_write_attempt_outputs_and_nothing_else() {
    let warehouse = Warehouse::create().await;
    let name = warehouse.create_table("db", "events").await;
    let table = warehouse
        .connect()
        .await
        .load_table(&name, true)
        .await
        .unwrap();
    let output = table
        .location
        .attempt_output_prefix(JobId::generate(), AttemptId::generate())
        .child("part-0.parquet")
        .unwrap();

    let store = ObjectStore::open(
        &rustfs_profile(),
        Some(CredentialCell::new(table.credentials.clone())),
    )
    .unwrap();
    store
        .write(&output, Bytes::from_static(b"x"))
        .await
        .unwrap();

    // Widen the client-side scope to the whole bucket: the store itself must
    // still refuse writes outside the table.
    let mut widened = table.credentials.clone();
    for credential in &mut widened {
        credential.prefix = Location::parse("s3://warehouse").unwrap();
    }
    let unscoped =
        ObjectStore::open(&rustfs_profile(), Some(CredentialCell::new(widened))).unwrap();
    let outside = Location::parse(&format!("s3://warehouse/{}-outside/x", warehouse.name)).unwrap();
    let denied = unscoped.write(&outside, Bytes::from_static(b"x")).await;

    assert!(store.exists(&output).await.unwrap());
    assert!(
        denied.is_err(),
        "vended credentials must be scoped to the table"
    );
}

#[tokio::test]
async fn refreshed_credentials_keep_working() {
    let warehouse = Warehouse::create().await;
    let name = warehouse.create_table("db", "events").await;
    let connection = warehouse.connect().await;
    let table = connection.load_table(&name, true).await.unwrap();

    let refreshed = connection.refresh_credentials(&table).await.unwrap();
    let cell = CredentialCell::new(Vec::new());
    let store = ObjectStore::open(&rustfs_profile(), Some(cell.clone())).unwrap();
    cell.replace(refreshed);
    let location = table.location.child("refresh-probe").unwrap();

    store
        .write(&location, Bytes::from_static(b"x"))
        .await
        .unwrap();
    assert!(store.exists(&location).await.unwrap());
}

#[tokio::test]
async fn aliases_of_one_table_report_the_same_identity() {
    let warehouse = Warehouse::create().await;
    let name = warehouse.create_table("db", "events").await;
    let first = warehouse.connect().await;
    let second = warehouse.connect().await;

    let a = first.load_table(&name, false).await.unwrap().observation();
    let b = second.load_table(&name, false).await.unwrap().observation();

    assert_eq!(a.table_uuid, b.table_uuid);
    assert_eq!(a.location, b.location);
}
