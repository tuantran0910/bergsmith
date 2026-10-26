//! Table definition contracts (BS-026.T3).
//!
//! The definition is metadata only: these tests drive it through the catalog
//! mock and assert the contract, the allow-list, and the permission boundary.

use super::*;

use std::collections::HashMap;
use std::sync::Arc;

use bergsmith_fixtures::{Commit, FileSpec, Fixture, Row, TableSpec, Value as FixtureValue, row};
use iceberg::spec::{
    NestedField, PrimitiveType, Schema, TableMetadata, TableMetadataBuilder, Transform, Type,
    UnboundPartitionSpec,
};
use mockito::ServerGuard;

/// A table with one data file in an identity spec and a second, unpartitioned
/// spec, so a definition must report both.
async fn evolved_table() -> Fixture {
    let schema = Schema::builder()
        .with_fields([
            Arc::new(NestedField::required(
                1,
                "id",
                Type::Primitive(PrimitiveType::Long),
            )),
            Arc::new(NestedField::optional(
                2,
                "tag",
                Type::Primitive(PrimitiveType::String),
            )),
        ])
        .build()
        .unwrap();
    let spec = UnboundPartitionSpec::builder()
        .add_partition_field(2, "tag", Transform::Identity)
        .unwrap()
        .build();
    let rows: Vec<Row> = (0..2_i64).map(|id| row![1 => id, 2 => "x"]).collect();
    TableSpec::new(schema)
        .partitioned_by(spec)
        .commit(
            Commit::new()
                .file(FileSpec::data("x0", rows.clone()).partition([FixtureValue::string("x")])),
        )
        .commit(
            Commit::new()
                .evolve_spec(UnboundPartitionSpec::builder().build())
                .file(FileSpec::data("u0", rows)),
        )
        .generate()
        .await
        .unwrap()
}

/// `metadata` with `properties` added, as a metadata-only commit would leave
/// it.
fn with_properties(metadata: &TableMetadata, properties: &[(&str, &str)]) -> TableMetadata {
    let mut values: HashMap<String, String> = metadata
        .properties()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    for (key, value) in properties {
        values.insert((*key).to_owned(), (*value).to_owned());
    }
    TableMetadataBuilder::new_from_metadata(metadata.clone(), None)
        .set_properties(values)
        .unwrap()
        .build()
        .unwrap()
        .metadata
}

/// Serves `metadata` for the `events` table.
async fn definition_mock(
    server: &mut ServerGuard,
    fixture: &Fixture,
    metadata: &TableMetadata,
) -> Mock {
    server
        .mock("GET", "/v1/namespaces/db/tables/events")
        .with_body(
            json!({
                "metadata-location": fixture.metadata_location(),
                "metadata": metadata,
            })
            .to_string(),
        )
        .create_async()
        .await
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn definition_reports_schema_specs_sort_order_and_allowed_properties(pool: PgPool) {
    let fixture = evolved_table().await;
    let metadata = with_properties(
        fixture.metadata(),
        &[
            ("write.format.default", "parquet"),
            ("write.target-file-size-bytes", "1048576"),
            ("custom.private", "not reported"),
        ],
    );
    let mut server = mockito::Server::new_async().await;
    let config = catalog_config_mock(&mut server).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme", &server.url(), fixture.metadata().uuid()).await;
    let load = definition_mock(&mut server, &fixture, &metadata).await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let uri = format!("/api/v1/tables/{}/definition", tenant.table);
    let (status, body) = admin.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["table_id"], tenant.table.to_string());
    assert_eq!(body["format_version"], "v2");

    let schema = &body["schema"];
    assert_eq!(schema["schema_id"], 0);
    let fields = schema["fields"].as_array().unwrap();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0]["id"], 1);
    assert_eq!(fields[0]["name"], "id");
    assert_eq!(fields[0]["type"], "long");
    assert_eq!(fields[0]["required"], true);
    assert_eq!(fields[0]["doc"], Value::Null);

    let specs = body["partition_specs"].as_array().unwrap();
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0]["spec_id"], 0);
    assert_eq!(specs[0]["default"], false);
    assert_eq!(specs[0]["fields"][0]["name"], "tag");
    assert_eq!(specs[0]["fields"][0]["source_id"], 2);
    assert_eq!(specs[0]["fields"][0]["transform"], "identity");
    assert_eq!(specs[1]["spec_id"], 1);
    assert_eq!(specs[1]["default"], true);
    assert_eq!(specs[1]["fields"].as_array().unwrap().len(), 0);

    assert_eq!(body["sort_order"]["order_id"], 0);
    assert_eq!(body["sort_order"]["fields"].as_array().unwrap().len(), 0);

    let properties = body["properties"].as_object().unwrap();
    assert_eq!(properties["write.format.default"], "parquet");
    assert_eq!(properties["write.target-file-size-bytes"], "1048576");
    assert!(
        !properties.contains_key("custom.private"),
        "a property outside the allow-list leaked: {properties:?}"
    );
    // A definition carries no location, credential, or connection setting.
    assert!(body.get("metadata_location").is_none());
    assert!(body.get("location").is_none());
    config.assert_async().await;
    load.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn definition_is_available_to_a_viewer(pool: PgPool) {
    let fixture = evolved_table().await;
    let mut server = mockito::Server::new_async().await;
    let config = catalog_config_mock(&mut server).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme", &server.url(), fixture.metadata().uuid()).await;
    let load = definition_mock(&mut server, &fixture, fixture.metadata()).await;
    drop(conn);
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;

    let uri = format!("/api/v1/tables/{}/definition", tenant.table);
    let (status, body) = viewer.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["schema"]["fields"].as_array().unwrap().len(), 2);
    config.assert_async().await;
    load.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_invisible_table_is_not_found(pool: PgPool) {
    let fixture = evolved_table().await;
    let mut server = mockito::Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_body("{}")
        .expect(0)
        .create_async()
        .await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme", &server.url(), fixture.metadata().uuid()).await;
    let outsider = support::user(&mut conn, tenant.organization, "bob").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, outsider, "bob").await;
    let uri = format!("/api/v1/tables/{}/definition", tenant.table);
    let (status, body) = browser.request("GET", &uri, None, None).await;
    // An inventory read hides existence: the same 404 as an unknown table.
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // Visibility is decided from the ledger, before any catalog request.
    config.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn definition_reports_a_changed_identity_as_a_conflict(pool: PgPool) {
    let fixture = evolved_table().await;
    let mut server = mockito::Server::new_async().await;
    let config = catalog_config_mock(&mut server).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme", &server.url(), fixture.metadata().uuid()).await;
    let replaced = foreign_metadata(Uuid::now_v7(), fixture.metadata().location());
    let load = definition_mock(&mut server, &fixture, &replaced).await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let uri = format!("/api/v1/tables/{}/definition", tenant.table);
    let (status, body) = admin.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "table_identity_changed");
    config.assert_async().await;
    load.assert_async().await;
}
