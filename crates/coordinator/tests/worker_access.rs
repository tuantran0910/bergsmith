//! Coordinator-owned catalog vending for worker grants and heartbeats.

use std::collections::BTreeMap;
use std::sync::Arc;

use bergsmith_catalog::{CatalogConnection, ConnectionConfig, TableName};
use bergsmith_common::ids::JobId;
use bergsmith_coordinator::worker_access::{CatalogWorkerAccess, WorkerTableBinding};
use bergsmith_coordinator::workers::WorkerAccess;
use bergsmith_fixtures::spike;
use bergsmith_storage::{S3Profile, StorageProfile};
use mockito::{Matcher, Server};
use serde_json::json;

#[tokio::test]
async fn catalog_access_keeps_credentials_in_memory_and_refreshes_them_on_every_load() {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let mut metadata = serde_json::to_value(fixture.metadata()).unwrap();
    metadata["location"] = json!("s3://warehouse/db/t");
    let mut server = Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .create_async()
        .await;
    let catalog = CatalogConnection::connect(&ConnectionConfig::new(server.url().parse().unwrap()))
        .await
        .unwrap();
    config.assert_async().await;
    let load = server
        .mock("GET", "/v1/namespaces/db/tables/t")
        .match_header(
            "x-iceberg-access-delegation",
            Matcher::Exact("vended-credentials".into()),
        )
        .with_body(
            json!({"metadata": metadata, "storage-credentials": [{
                "prefix": "s3://warehouse/db/t", "config": {"s3.access-key-id": "test-access",
                    "s3.secret-access-key": "test-secret", "s3.session-token": "test-session"}
            }]})
            .to_string(),
        )
        .expect(2)
        .create_async()
        .await;
    let job = JobId::generate();
    let provider = CatalogWorkerAccess::new(BTreeMap::from([(
        job,
        WorkerTableBinding {
            catalog: Arc::new(catalog),
            table: TableName {
                namespace: vec!["db".into()],
                name: "t".into(),
            },
            profile: StorageProfile::S3(S3Profile {
                endpoint: None,
                region: "us-east-1".into(),
                path_style_access: false,
            }),
        },
    )]));
    for _ in 0..2 {
        let access = provider.access(job).await.unwrap();
        let credentials = access.credentials.unwrap();
        assert_eq!(credentials.prefix, "s3://warehouse/db/t");
        assert_eq!(credentials.properties["s3.session-token"], "test-session");
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("test-secret"));
        assert!(!debug.contains("test-session"));
    }
    load.assert_async().await;
    assert!(provider.access(JobId::generate()).await.is_err());
}

#[tokio::test]
async fn local_storage_access_needs_no_vended_credentials() {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let mut server = Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .create_async()
        .await;
    let catalog = CatalogConnection::connect(&ConnectionConfig::new(server.url().parse().unwrap()))
        .await
        .unwrap();
    config.assert_async().await;
    let load = server
        .mock("GET", "/v1/namespaces/db/tables/t")
        .with_body(json!({"metadata": fixture.metadata()}).to_string())
        .create_async()
        .await;
    let job = JobId::generate();
    let provider = CatalogWorkerAccess::new(BTreeMap::from([(
        job,
        WorkerTableBinding {
            catalog: Arc::new(catalog),
            table: TableName {
                namespace: vec!["db".into()],
                name: "t".into(),
            },
            profile: StorageProfile::LocalFs,
        },
    )]));
    let access = provider.access(job).await.unwrap();
    assert!(access.credentials.is_none());
    assert_eq!(access.metadata.uuid(), fixture.metadata().uuid());
    load.assert_async().await;
}
