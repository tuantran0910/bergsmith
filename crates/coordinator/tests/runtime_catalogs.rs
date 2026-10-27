//! Runtime table access remains available after connection disable.

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

use std::sync::Arc;

use bergsmith_catalog::{ObservedTable, TableName};
use bergsmith_common::ids::JobId;
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_coordinator::inventory::record_observation;
use bergsmith_coordinator::runtime::catalogs::RuntimeCatalogs;
use bergsmith_coordinator::runtime::config::RuntimeProfiles;
use bergsmith_fixtures::{Commit, FileSpec, TableSpec, row};
use bergsmith_persistence::Database;
use bergsmith_persistence::jobs::{self, NewJob};
use bergsmith_storage::{Location, StorageProfile, StorageProfiles};
use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
use sqlx::PgPool;

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn running_work_can_load_a_table_through_its_only_disabled_alias(pool: PgPool) {
    let schema = Schema::builder()
        .with_fields([Arc::new(NestedField::required(
            1,
            "id",
            Type::Primitive(PrimitiveType::Long),
        ))])
        .build()
        .unwrap();
    let fixture = TableSpec::new(schema)
        .commit(Commit::new().file(FileSpec::data("data", [row![1 => 1_i64]])))
        .generate()
        .await
        .unwrap();
    let mut server = mockito::Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .expect(1)
        .create_async()
        .await;
    let table = server.mock("GET","/v1/namespaces/db/tables/rewrite").with_body(serde_json::json!({"metadata":fixture.metadata(),"metadata-location":fixture.metadata_location(),"config":{}}).to_string()).expect(1).create_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    sqlx::query("UPDATE catalog_connections SET uri=$2, warehouse=NULL, credential_ref=NULL, enabled=false WHERE id=$1")
        .bind(tenant.connection.as_uuid()).bind(server.url()).execute(&mut *conn).await.unwrap();
    let table_id = record_observation(
        &mut conn,
        tenant.organization,
        tenant.connection,
        &ObservedTable {
            name: TableName {
                namespace: vec!["db".into()],
                name: "rewrite".into(),
            },
            table_uuid: fixture.metadata().uuid(),
            location: Location::parse(fixture.metadata().location()).unwrap(),
        },
    )
    .await
    .unwrap();
    let job = jobs::create_job(
        &mut conn,
        &NewJob {
            id: JobId::generate(),
            organization_id: tenant.organization,
            table_id,
            operation: JobOperation::Compact,
            retry_of: None,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();
    support::force_job_state(&mut conn, job.id, JobState::Running).await;
    let job = jobs::job_by_id(&mut conn, job.id).await.unwrap();
    drop(conn);
    let catalogs = RuntimeCatalogs::new(
        Database::from_pool(pool),
        RuntimeProfiles {
            default: StorageProfiles::new([StorageProfile::LocalFs]),
            connections: std::collections::BTreeMap::new(),
        },
    );
    let binding = catalogs.binding_for_job(&job).await.unwrap();
    assert_eq!(binding.connection.id, tenant.connection);
    assert!(!binding.connection.enabled);
    assert_eq!(binding.loaded.metadata.uuid(), fixture.metadata().uuid());
    config.assert_async().await;
    table.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_paused_connection_is_not_chosen_over_one_that_allows_maintenance(pool: PgPool) {
    let schema = Schema::builder()
        .with_fields([Arc::new(NestedField::required(
            1,
            "id",
            Type::Primitive(PrimitiveType::Long),
        ))])
        .build()
        .unwrap();
    let fixture = TableSpec::new(schema)
        .commit(Commit::new().file(FileSpec::data("data", [row![1 => 1_i64]])))
        .generate()
        .await
        .unwrap();
    let mut server = mockito::Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .expect(1)
        .create_async()
        .await;
    let table = server.mock("GET","/v1/namespaces/db/tables/rewrite").with_body(serde_json::json!({"metadata":fixture.metadata(),"metadata-location":fixture.metadata_location(),"config":{}}).to_string()).expect(1).create_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    sqlx::query(
        "UPDATE catalog_connections SET uri=$2, warehouse=NULL, credential_ref=NULL WHERE id=$1",
    )
    .bind(tenant.connection.as_uuid())
    .bind(server.url())
    .execute(&mut *conn)
    .await
    .unwrap();
    // A second connection serves the same table but has maintenance paused.
    let paused = support::connection(&mut conn, tenant.organization, "paused").await;
    sqlx::query("UPDATE catalog_connections SET uri=$2, warehouse=NULL, credential_ref=NULL, maintenance_enabled=false WHERE id=$1")
        .bind(paused.as_uuid()).bind(server.url()).execute(&mut *conn).await.unwrap();
    let observed = ObservedTable {
        name: TableName {
            namespace: vec!["db".into()],
            name: "rewrite".into(),
        },
        table_uuid: fixture.metadata().uuid(),
        location: Location::parse(fixture.metadata().location()).unwrap(),
    };
    let table_id = record_observation(&mut conn, tenant.organization, tenant.connection, &observed)
        .await
        .unwrap();
    record_observation(&mut conn, tenant.organization, paused, &observed)
        .await
        .unwrap();
    let job = jobs::create_job(
        &mut conn,
        &NewJob {
            id: JobId::generate(),
            organization_id: tenant.organization,
            table_id,
            operation: JobOperation::Compact,
            retry_of: None,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();
    support::force_job_state(&mut conn, job.id, JobState::Running).await;
    let job = jobs::job_by_id(&mut conn, job.id).await.unwrap();
    drop(conn);
    let catalogs = RuntimeCatalogs::new(
        Database::from_pool(pool),
        RuntimeProfiles {
            default: StorageProfiles::new([StorageProfile::LocalFs]),
            connections: std::collections::BTreeMap::new(),
        },
    );
    let binding = catalogs.binding_for_job(&job).await.unwrap();
    assert_eq!(
        binding.connection.id, tenant.connection,
        "the connection that allows maintenance is chosen"
    );
    config.assert_async().await;
    table.assert_async().await;
}
