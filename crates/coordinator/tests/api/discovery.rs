//! Catalog discovery, registration, and removal (BS-022.T4).

use super::*;
use bergsmith_fixtures::{Commit, FileSpec, TableSpec, row};
use bergsmith_persistence::tables::{self, TableAlias};
use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
use mockito::{Matcher, Mock};
use std::sync::Arc;
use uuid::Uuid;

/// Points the tenant's connection at a mock catalog with no credentials.
async fn point_at(conn: &mut PgConnection, tenant: &support::Tenant, server: &mockito::Server) {
    sqlx::query(
        "UPDATE catalog_connections SET uri=$2, warehouse=NULL, credential_ref=NULL WHERE id=$1",
    )
    .bind(tenant.connection.as_uuid())
    .bind(server.url())
    .execute(&mut *conn)
    .await
    .unwrap();
}

/// The body a catalog answers a table load with, and the table's UUID.
async fn table_body() -> (Value, Uuid) {
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
    // Each fixture declares the same table UUID; a catalog serves distinct
    // tables, so give every body its own.
    let table_uuid = Uuid::now_v7();
    let mut body = json!({
        "metadata": fixture.metadata(),
        "metadata-location": fixture.metadata_location(),
        "config": {},
    });
    body["metadata"]["table-uuid"] = json!(table_uuid.to_string());
    (body, table_uuid)
}

/// The catalog configuration every discovery test answers.
async fn catalog_config(server: &mut mockito::Server, body: &str) -> Mock {
    server
        .mock("GET", "/v1/config")
        .with_body(body)
        .create_async()
        .await
}

fn path(tenant: &support::Tenant) -> String {
    format!("/api/v1/catalogs/{}", tenant.connection)
}

/// The revision of one alias row, for a removal that names what it observed.
async fn revision(conn: &mut PgConnection, tenant: &support::Tenant, name: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT revision FROM table_aliases WHERE catalog_connection_id=$1 AND table_name=$2",
    )
    .bind(tenant.connection.as_uuid())
    .bind(name)
    .fetch_one(&mut *conn)
    .await
    .unwrap()
}

/// A catalog that answers a table load only when the test releases it.
///
/// The configuration is answered at once, so a client can connect; the load
/// reports that it arrived and waits. No test depends on a timer, so a slow
/// machine cannot change the outcome.
async fn slow_load_catalog(
    body: Value,
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<()>,
    Arc<tokio::sync::Semaphore>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (arrived, arrived_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let shared = release.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let arrived = arrived.clone();
            let release = shared.clone();
            let body = body.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                let Ok(read) = stream.read(&mut buffer).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let (payload, waits) = if request.starts_with("GET /v1/config") {
                    (r#"{"defaults":{},"overrides":{}}"#.to_owned(), false)
                } else {
                    let _ = arrived.send(());
                    (body.to_string(), true)
                };
                if waits {
                    let _ = release.acquire().await;
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://{address}"), arrived_rx, release)
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_browse_reads_the_namespaces_under_a_parent(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    drop(conn);
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    let listing = server
        .mock("GET", "/v1/namespaces")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("parent".into(), "db".into()),
            Matcher::UrlEncoded("pageSize".into(), "10".into()),
        ]))
        .with_body(r#"{"namespaces":[["db","west"],["db","east"]],"next-page-token":null}"#)
        .expect(1)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, body) = browser
        .request(
            "GET",
            &format!("{}/namespaces?parent=db&limit=10", path(&tenant)),
            None,
            None,
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["items"],
        json!([
            {"name": "db.east", "levels": ["db", "east"]},
            {"name": "db.west", "levels": ["db", "west"]},
        ])
    );
    assert_eq!(body["next_cursor"], Value::Null);
    assert_eq!(body["skipped"], 0);
    listing.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_browse_reaches_every_name_of_a_namespace_larger_than_one_page(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    drop(conn);
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    // 201 tables: one page cannot hold them, so the last table is reachable
    // only through the continuation the answer carries.
    let first_page: Vec<Value> = (0..200)
        .map(|index| json!({"namespace": ["db"], "name": format!("table-{index:03}")}))
        .collect();
    let first_page = server
        .mock("GET", "/v1/namespaces/db/tables")
        .match_query(Matcher::UrlEncoded("pageSize".into(), "200".into()))
        .with_body(json!({"identifiers": first_page, "next-page-token": "p2"}).to_string())
        .expect(1)
        .create_async()
        .await;
    let second_page = server
        .mock("GET", "/v1/namespaces/db/tables")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("pageToken".into(), "p2".into()),
            Matcher::UrlEncoded("pageSize".into(), "200".into()),
        ]))
        .with_body(
            r#"{"identifiers":[{"namespace":["db"],"name":"table-last"}],"next-page-token":null}"#,
        )
        .expect(1)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let listing = format!("{}/tables?namespace=db&limit=200", path(&tenant));

    let (status, first) = browser.request("GET", &listing, None, None).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["items"].as_array().unwrap().len(), 200);
    let cursor = first["next_cursor"].as_str().unwrap();

    let (status, second) = browser
        .request("GET", &format!("{listing}&cursor={cursor}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["items"][0]["name"], "table-last");
    assert_eq!(second["next_cursor"], Value::Null);
    first_page.assert_async().await;
    second_page.assert_async().await;

    // The cursor is bound to the listing it was issued for.
    let (status, error) = browser
        .request(
            "GET",
            &format!(
                "{}/tables?namespace=other&limit=200&cursor={cursor}",
                path(&tenant)
            ),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_table_browse_marks_the_names_that_are_registered(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    let events_revision = revision(&mut conn, &tenant, "events").await;
    drop(conn);
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    let listing = server
        .mock("GET", "/v1/namespaces/db/tables")
        .match_query(Matcher::UrlEncoded("pageSize".into(), "50".into()))
        .with_body(
            r#"{"identifiers":[{"namespace":["db"],"name":"events"},{"namespace":["db"],"name":"audit"}],"next-page-token":null}"#,
        )
        .expect(1)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, body) = browser
        .request(
            "GET",
            &format!("{}/tables?namespace=db", path(&tenant)),
            None,
            None,
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["items"],
        json!([
            {
                "namespace": ["db"],
                "name": "audit",
                "registered": false,
                "table_id": null,
                "revision": null,
            },
            {
                "namespace": ["db"],
                "name": "events",
                "registered": true,
                "table_id": tenant.table,
                "revision": events_revision,
            },
        ])
    );
    assert_eq!(body["next_cursor"], Value::Null);
    assert_eq!(body["skipped"], 0);
    listing.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_catalog_answering_another_namespace_is_refused(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    drop(conn);
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    // `db.events` is registered; the catalog answers with `other.events`.
    let listing = server
        .mock("GET", "/v1/namespaces/db/tables")
        .match_query(Matcher::Any)
        .with_body(
            r#"{"identifiers":[{"namespace":["other"],"name":"events"}],"next-page-token":null}"#,
        )
        .expect(2)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    // Browsing refuses the answer rather than marking `other.events` with
    // `db.events`'s identity.
    let (status, error) = browser
        .request(
            "GET",
            &format!("{}/tables?namespace=db", path(&tenant)),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["reason"], "catalog_namespace_mismatch");

    // Registering the namespace would record a name the request never named.
    let (status, error) = browser
        .request(
            "POST",
            &format!("{}/registrations", path(&tenant)),
            Some(json!({"namespace": ["db"]})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["reason"], "catalog_namespace_mismatch");
    let other: i64 = sqlx::query_scalar("SELECT count(*) FROM table_aliases WHERE namespace=$1")
        .bind(vec!["other".to_owned()])
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(other, 0, "nothing outside the request is recorded");
    listing.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unaddressable_name_is_skipped_and_refuses_a_namespace_registration(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    drop(conn);
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    // A 4 MiB level and a control character: neither can be addressed, so
    // neither is returned or registered.
    let body = json!({
        "identifiers": [
            {"namespace": ["db"], "name": "orders"},
            {"namespace": ["db"], "name": "a".repeat(4 * 1024 * 1024)},
            {"namespace": ["db"], "name": "bad\nname"},
        ],
        "next-page-token": null,
    });
    let listing = server
        .mock("GET", "/v1/namespaces/db/tables")
        .match_query(Matcher::Any)
        .with_body(body.to_string())
        .expect(2)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, browse) = browser
        .request(
            "GET",
            &format!("{}/tables?namespace=db", path(&tenant)),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{browse}");
    assert_eq!(browse["items"][0]["name"], "orders");
    assert_eq!(browse["skipped"], 2);

    // A namespace registration cannot silently leave those two out.
    let (status, error) = browser
        .request(
            "POST",
            &format!("{}/registrations", path(&tenant)),
            Some(json!({"namespace": ["db"]})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["reason"], "registration_incomplete");
    let orders: i64 =
        sqlx::query_scalar("SELECT count(*) FROM table_aliases WHERE table_name='orders'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(orders, 0);
    listing.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_page_too_large_to_carry_is_reported(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    drop(conn);
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    let identifiers: Vec<Value> = (0..=max_listing_items())
        .map(|index| json!({"namespace": ["db"], "name": format!("table-{index}")}))
        .collect();
    let listing = server
        .mock("GET", "/v1/namespaces/db/tables")
        .match_query(Matcher::Any)
        .with_body(json!({"identifiers": identifiers, "next-page-token": null}).to_string())
        .expect(1)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, error) = browser
        .request(
            "GET",
            &format!("{}/tables?namespace=db", path(&tenant)),
            None,
            None,
        )
        .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{error}");
    assert_eq!(error["reason"], "catalog_page_too_large");
    listing.assert_async().await;
}

/// One more item than one catalog page may carry.
fn max_listing_items() -> usize {
    bergsmith_catalog::MAX_LISTING_ITEMS
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn registering_a_table_is_idempotent_and_audited_once(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    let (loaded, table_uuid) = table_body().await;
    let load = server
        .mock("GET", "/v1/namespaces/db/tables/orders")
        .with_body(loaded.to_string())
        .expect(2)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let registrations = format!("{}/registrations", path(&tenant));
    let request = json!({"namespace": ["db"], "table": "orders"});

    let (status, created) = browser
        .request("POST", &registrations, Some(request.clone()), None)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let registered = &created["registered"][0];
    assert_eq!(registered["namespace"], json!(["db"]));
    assert_eq!(registered["name"], "orders");
    assert_eq!(registered["created"], true);
    // The answer names the canonical table the alias resolves to; the
    // identity keeps the catalog's table UUID.
    let table_id: Uuid = sqlx::query_scalar(
        "SELECT table_identity_id FROM table_aliases WHERE catalog_connection_id=$1 AND table_name='orders'",
    )
    .bind(tenant.connection.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(registered["table_id"], json!(table_id.to_string()));
    let identity_uuid: Uuid =
        sqlx::query_scalar("SELECT table_uuid FROM table_identities WHERE id=$1")
            .bind(table_id)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(identity_uuid, table_uuid);
    let table_id = table_id.to_string();

    // The same name again answers with the identity the first request
    // recorded, and records nothing new.
    let (status, repeated) = browser
        .request("POST", &registrations, Some(request), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{repeated}");
    assert_eq!(repeated["registered"][0]["table_id"], json!(table_id));
    assert_eq!(repeated["registered"][0]["created"], false);
    load.assert_async().await;

    let audits: Vec<(String, Value)> = sqlx::query_as(
        "SELECT action, detail FROM audit_events WHERE action='catalog.tables_registered' ORDER BY occurred_at",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        audits.len(),
        1,
        "a repeated request changes nothing: {audits:?}"
    );
    assert_eq!(audits[0].1["table"], "orders");
    assert_eq!(audits[0].1["created"], true);
    assert_eq!(audits[0].1["table_id"], json!(table_id));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn registering_a_namespace_records_every_table_or_none(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    drop(conn);
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    let (first, _first_uuid) = table_body().await;
    let (second, _second_uuid) = table_body().await;
    let listing = server
        .mock("GET", "/v1/namespaces/db/tables")
        .match_query(Matcher::Any)
        .with_body(
            r#"{"identifiers":[{"namespace":["db"],"name":"orders"},{"namespace":["db"],"name":"refunds"}],"next-page-token":null}"#,
        )
        .expect(3)
        .create_async()
        .await;
    let orders = server
        .mock("GET", "/v1/namespaces/db/tables/orders")
        .with_body(first.to_string())
        .expect(3)
        .create_async()
        .await;
    let refunds = server
        .mock("GET", "/v1/namespaces/db/tables/refunds")
        .with_body(second.to_string())
        .expect(1)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let registrations = format!("{}/registrations", path(&tenant));

    let (status, body) = browser
        .request(
            "POST",
            &registrations,
            Some(json!({"namespace": ["db"]})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let registered = body["registered"].as_array().unwrap();
    assert_eq!(registered.len(), 2, "{body}");
    for (entry, name) in registered.iter().zip(["orders", "refunds"]) {
        assert_eq!(entry["namespace"], json!(["db"]));
        assert_eq!(entry["name"], name);
        assert_eq!(entry["created"], true);
        Uuid::parse_str(entry["table_id"].as_str().unwrap()).unwrap();
    }
    assert_ne!(registered[0]["table_id"], registered[1]["table_id"]);
    let audit: Value = sqlx::query_scalar(
        "SELECT detail FROM audit_events WHERE action='catalog.tables_registered'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit["namespace"], json!(["db"]));
    assert_eq!(audit["requested"], 2);
    assert_eq!(audit["created"], 2);

    // A table the catalog cannot read leaves the whole namespace unregistered.
    let refunds_missing = server
        .mock("GET", "/v1/namespaces/db/tables/refunds")
        .with_status(404)
        .expect(1)
        .create_async()
        .await;
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("DELETE FROM table_aliases WHERE table_name IN ('orders','refunds')")
        .execute(&mut *conn)
        .await
        .unwrap();
    let (status, error) = browser
        .request(
            "POST",
            &registrations,
            Some(json!({"namespace": ["db"]})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{error}");
    assert_eq!(error["reason"], "catalog_table_not_found");
    let registered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM table_aliases WHERE table_name IN ('orders','refunds')",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(registered, 0, "one unreadable table registers nothing");
    refunds_missing.assert_async().await;

    // An alias that now reaches a different table fails the whole request,
    // after the first table loaded and before any alias is written.
    let (third, _third_uuid) = table_body().await;
    let _third_load = server
        .mock("GET", "/v1/namespaces/db/tables/refunds")
        .with_body(third.to_string())
        .expect(1)
        .create_async()
        .await;
    tables::resolve_table(
        &mut conn,
        tenant.organization,
        &TableAlias {
            connection_id: tenant.connection,
            namespace: vec!["db".to_owned()],
            name: "refunds".to_owned(),
        },
        Uuid::now_v7(),
    )
    .await
    .unwrap();
    let (status, conflict) = browser
        .request(
            "POST",
            &registrations,
            Some(json!({"namespace": ["db"]})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["reason"], "table_identity_conflict");
    let orders_registered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM table_aliases WHERE catalog_connection_id=$1 AND table_name='orders'",
    )
    .bind(tenant.connection.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(orders_registered, 0, "one conflicting alias writes nothing");
    orders.assert_async().await;
    refunds.assert_async().await;
    listing.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_namespace_registration_follows_every_page(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    drop(conn);
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    let (first, _first_uuid) = table_body().await;
    let (second, _second_uuid) = table_body().await;
    // Two tables, one per page: a namespace that spans pages is one request.
    let _first_page = server
        .mock("GET", "/v1/namespaces/db/tables")
        .match_query(Matcher::UrlEncoded("pageSize".into(), "101".into()))
        .with_body(
            r#"{"identifiers":[{"namespace":["db"],"name":"orders"}],"next-page-token":"p2"}"#,
        )
        .expect(1)
        .create_async()
        .await;
    let second_page = server
        .mock("GET", "/v1/namespaces/db/tables")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("pageToken".into(), "p2".into()),
            Matcher::UrlEncoded("pageSize".into(), "101".into()),
        ]))
        .with_body(
            r#"{"identifiers":[{"namespace":["db"],"name":"refunds"}],"next-page-token":null}"#,
        )
        .expect(1)
        .create_async()
        .await;
    let orders = server
        .mock("GET", "/v1/namespaces/db/tables/orders")
        .with_body(first.to_string())
        .expect(1)
        .create_async()
        .await;
    let refunds = server
        .mock("GET", "/v1/namespaces/db/tables/refunds")
        .with_body(second.to_string())
        .expect(1)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, body) = browser
        .request(
            "POST",
            &format!("{}/registrations", path(&tenant)),
            Some(json!({"namespace": ["db"]})),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["registered"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["orders", "refunds"]
    );
    second_page.assert_async().await;
    orders.assert_async().await;
    refunds.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_namespace_registration_checks_every_page(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    drop(conn);
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    // The second page holds a name the API cannot address and still claims to
    // have more, so the namespace can never be enumerated completely.
    let _first_page = server
        .mock("GET", "/v1/namespaces/db/tables")
        .match_query(Matcher::UrlEncoded("pageSize".into(), "101".into()))
        .with_body(
            r#"{"identifiers":[{"namespace":["db"],"name":"orders"}],"next-page-token":"p2"}"#,
        )
        .expect(1)
        .create_async()
        .await;
    let second_page = server
        .mock("GET", "/v1/namespaces/db/tables")
        .match_query(Matcher::UrlEncoded("pageToken".into(), "p2".into()))
        .with_body(
            r#"{"identifiers":[{"namespace":["db"],"name":"bad\nname"}],"next-page-token":"p3"}"#,
        )
        .expect(1)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, error) = browser
        .request(
            "POST",
            &format!("{}/registrations", path(&tenant)),
            Some(json!({"namespace": ["db"]})),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["reason"], "registration_incomplete");
    second_page.assert_async().await;
    let registered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM table_aliases WHERE table_name IN ('orders','refunds')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        registered, 0,
        "a namespace that cannot be enumerated writes nothing"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_dropped_catalog_name_still_reports_its_revision(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    let (loaded, _uuid) = table_body().await;
    let _load = server
        .mock("GET", "/v1/namespaces/db/tables/orders")
        .with_body(loaded.to_string())
        .expect(1)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, created) = browser
        .request(
            "POST",
            &format!("{}/registrations", path(&tenant)),
            Some(json!({"namespace": ["db"], "table": "orders"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let table_id = created["registered"][0]["table_id"]
        .as_str()
        .unwrap()
        .to_owned();
    // The catalog drops the table, so no browse can report the name again.
    let _empty = server
        .mock("GET", "/v1/namespaces/db/tables")
        .match_query(Matcher::Any)
        .with_body(r#"{"identifiers":[],"next-page-token":null}"#)
        .expect(1)
        .create_async()
        .await;
    let (status, browse) = browser
        .request(
            "GET",
            &format!("{}/tables?namespace=db", path(&tenant)),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{browse}");
    assert_eq!(browse["items"], json!([]));

    // The ledger still knows the name, and the table resource reports the
    // revision a removal needs.
    let (status, table) = browser
        .request("GET", &format!("/api/v1/tables/{table_id}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{table}");
    let alias = &table["aliases"][0];
    assert_eq!(alias["name"], "orders");
    assert_eq!(alias["namespace"], json!(["db"]));
    let revision = alias["revision"].as_i64().unwrap();
    assert!(revision > 0, "{table}");

    let (status, removed) = browser
        .request(
            "DELETE",
            &format!(
                "{}/registrations?namespace=db&table=orders&revision={revision}",
                path(&tenant)
            ),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_eq!(removed["table_id"], json!(table_id));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_namespace_over_the_registration_bound_is_refused(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    drop(conn);
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    let identifiers: Vec<Value> = (0..=bergsmith_coordinator::discovery::MAX_REGISTRATION_TABLES)
        .map(|index| json!({"namespace": ["db"], "name": format!("table-{index}")}))
        .collect();
    let listing = server
        .mock("GET", "/v1/namespaces/db/tables")
        .match_query(Matcher::Any)
        .with_body(json!({"identifiers": identifiers, "next-page-token": null}).to_string())
        .expect(1)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, error) = browser
        .request(
            "POST",
            &format!("{}/registrations", path(&tenant)),
            Some(json!({"namespace": ["db"]})),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["reason"], "registration_too_large");
    listing.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn unregistering_removes_one_name_and_keeps_the_table(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    let (loaded, table_uuid) = table_body().await;
    let _load = server
        .mock("GET", "/v1/namespaces/db/tables/orders")
        .with_body(loaded.to_string())
        .expect(1)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let registrations = format!("{}/registrations", path(&tenant));
    let (status, created) = browser
        .request(
            "POST",
            &registrations,
            Some(json!({"namespace": ["db"], "table": "orders"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let table_id: Uuid = sqlx::query_scalar(
        "SELECT table_identity_id FROM table_aliases WHERE catalog_connection_id=$1 AND table_name='orders'",
    )
    .bind(tenant.connection.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    let revision = revision(&mut conn, &tenant, "orders").await;

    let removal = format!("{registrations}?namespace=db&table=orders&revision={revision}");
    let (status, body) = browser.request("DELETE", &removal, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["table_id"], json!(table_id.to_string()));

    let aliases: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM table_aliases WHERE catalog_connection_id=$1 AND table_name='orders'",
    )
    .bind(tenant.connection.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(aliases, 0);
    // The canonical table stays: jobs and publication evidence reference it.
    let identities: i64 =
        sqlx::query_scalar("SELECT count(*) FROM table_identities WHERE table_uuid=$1")
            .bind(table_uuid)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(identities, 1);
    let audit: Value = sqlx::query_scalar(
        "SELECT detail FROM audit_events WHERE action='catalog.table_unregistered'",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(audit["table"], "orders");
    assert_eq!(audit["namespace"], json!(["db"]));

    let (status, missing) = browser.request("DELETE", &removal, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_delayed_removal_does_not_remove_a_replacement(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    let (loaded, _uuid) = table_body().await;
    let _load = server
        .mock("GET", "/v1/namespaces/db/tables/orders")
        .with_body(loaded.to_string())
        .expect(2)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let registrations = format!("{}/registrations", path(&tenant));
    let request = json!({"namespace": ["db"], "table": "orders"});
    let (status, created) = browser
        .request("POST", &registrations, Some(request.clone()), None)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let observed = revision(&mut conn, &tenant, "orders").await;
    let removal = format!("{registrations}?namespace=db&table=orders&revision={observed}");

    // The operator removes what it saw, and a replacement takes the name.
    let (status, body) = browser.request("DELETE", &removal, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, created) = browser
        .request("POST", &registrations, Some(request), None)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    // The delayed removal of the row the operator saw changes nothing.
    let (status, error) = browser.request("DELETE", &removal, None, None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["reason"], "revision_conflict");
    let kept: i64 =
        sqlx::query_scalar("SELECT count(*) FROM table_aliases WHERE table_name='orders'")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(kept, 1, "the replacement survives");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn unregistering_a_table_with_work_in_flight_is_refused(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let _job = support::job(&mut conn, &tenant).await;
    let revision = revision(&mut conn, &tenant, "events").await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, error) = browser
        .request(
            "DELETE",
            &format!(
                "{}/registrations?namespace=db&table=events&revision={revision}",
                path(&tenant)
            ),
            None,
            None,
        )
        .await;

    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["reason"], "table_in_use");
    let aliases: i64 =
        sqlx::query_scalar("SELECT count(*) FROM table_aliases WHERE table_name='events'")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(aliases, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn discovery_requires_connection_management(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let (viewer, _grant) = scoped(&pool, &tenant, Role::Viewer, GrantScope::organization()).await;
    let catalog = path(&tenant);

    for (method, uri, body) in [
        ("GET", format!("{catalog}/namespaces"), None),
        ("GET", format!("{catalog}/tables?namespace=db"), None),
        (
            "POST",
            format!("{catalog}/registrations"),
            Some(json!({"namespace": ["db"], "table": "orders"})),
        ),
        (
            "DELETE",
            format!("{catalog}/registrations?namespace=db&table=events&revision=1"),
            None,
        ),
    ] {
        let (status, error) = viewer.request(method, &uri, body, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {error}");
        assert_eq!(error["reason"], "permission_denied");
    }
    let denials: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events WHERE action='api.permission_denied' AND detail->>'permission'='connections:manage'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(denials, 4, "every refusal is audited");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unusable_discovery_request_is_refused(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let catalog = path(&tenant);

    for (method, uri, body) in [
        ("GET", format!("{catalog}/namespaces?unknown=db"), None),
        ("GET", format!("{catalog}/namespaces?limit=0"), None),
        ("GET", format!("{catalog}/namespaces?limit=201"), None),
        (
            "GET",
            format!("{catalog}/namespaces?parent=db&table=t"),
            None,
        ),
        (
            "GET",
            format!("{catalog}/namespaces?parent=db&revision=1"),
            None,
        ),
        ("GET", format!("{catalog}/namespaces?parent="), None),
        (
            "GET",
            format!("{catalog}/namespaces?cursor=not-base64"),
            None,
        ),
        ("GET", format!("{catalog}/tables"), None),
        (
            "POST",
            format!("{catalog}/registrations"),
            Some(json!({"namespace": [], "table": "orders"})),
        ),
        (
            "POST",
            format!("{catalog}/registrations"),
            Some(json!({"namespace": ["db"], "table": ""})),
        ),
        (
            "POST",
            format!("{catalog}/registrations"),
            Some(json!({"namespace": ["db"], "unknown": "orders"})),
        ),
        (
            "DELETE",
            format!("{catalog}/registrations?namespace=db"),
            None,
        ),
        (
            "DELETE",
            format!("{catalog}/registrations?namespace=db&table=events"),
            None,
        ),
        (
            "DELETE",
            format!("{catalog}/registrations?namespace=db&table=events&revision=-1"),
            None,
        ),
        (
            "DELETE",
            format!("{catalog}/registrations?table=events&revision=1"),
            None,
        ),
    ] {
        let (status, error) = browser.request(method, &uri, body, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {uri}: {error}");
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn registration_rolls_back_when_the_audit_fails(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    let (loaded, _uuid) = table_body().await;
    let _load = server
        .mock("GET", "/v1/namespaces/db/tables/orders")
        .with_body(loaded.to_string())
        .expect(1)
        .create_async()
        .await;
    sqlx::query("CREATE FUNCTION reject_catalog_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test audit unavailable'; END $$").execute(&mut *conn).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_catalog_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_catalog_audit()").execute(&mut *conn).await.unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, error) = browser
        .request(
            "POST",
            &format!("{}/registrations", path(&tenant)),
            Some(json!({"namespace": ["db"], "table": "orders"})),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{error}");
    let mut conn = pool.acquire().await.unwrap();
    let aliases: i64 =
        sqlx::query_scalar("SELECT count(*) FROM table_aliases WHERE table_name='orders'")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(aliases, 0, "the alias and its audit commit together");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_registration_is_not_recorded_when_the_connection_changes(pool: PgPool) {
    let (loaded, _uuid) = table_body().await;
    let (url, mut arrived, release) = slow_load_catalog(loaded).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    sqlx::query(
        "UPDATE catalog_connections SET uri=$2, warehouse=NULL, credential_ref=NULL WHERE id=$1",
    )
    .bind(tenant.connection.as_uuid())
    .bind(&url)
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let registrations = format!("{}/registrations", path(&tenant));

    // The registration is waiting for the catalog when the connection is
    // re-pointed at another catalog.
    let registering = {
        let browser = browser.clone();
        let registrations = registrations.clone();
        tokio::spawn(async move {
            browser
                .request(
                    "POST",
                    &registrations,
                    Some(json!({"namespace": ["db"], "table": "orders"})),
                    None,
                )
                .await
        })
    };
    arrived
        .recv()
        .await
        .expect("the catalog was asked for the table");
    let (status, edited) = browser
        .request(
            "PATCH",
            &format!("/api/v1/catalogs/{}", tenant.connection),
            Some(json!({"expected_revision":0,"uri":"https://other.example/catalog"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{edited}");

    // The catalog answers only now, describing a connection that is gone.
    release.add_permits(1);
    let (status, conflict) = registering.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["reason"], "catalog_connection_changed");
    let mut conn = pool.acquire().await.unwrap();
    let aliases: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM table_aliases WHERE catalog_connection_id=$1 AND table_name='orders'",
    )
    .bind(tenant.connection.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(aliases, 0, "a changed connection records nothing");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_catalog_without_namespace_listing_is_reported(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    drop(conn);
    let _config = catalog_config(
        &mut server,
        r#"{"defaults":{},"overrides":{},"endpoints":[
            "GET /v1/{prefix}/namespaces/{namespace}/tables",
            "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}",
            "POST /v1/{prefix}/namespaces/{namespace}/tables/{table}"]}"#,
    )
    .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, error) = browser
        .request("GET", &format!("{}/namespaces", path(&tenant)), None, None)
        .await;

    assert_eq!(status, StatusCode::CONFLICT, "{error}");
    assert_eq!(error["reason"], "catalog_unsupported");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unreachable_catalog_is_retryable(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    point_at(&mut conn, &tenant, &server).await;
    drop(conn);
    let _config = catalog_config(&mut server, r#"{"defaults":{},"overrides":{}}"#).await;
    let _load = server
        .mock("GET", "/v1/namespaces/db/tables/orders")
        .with_status(503)
        .expect(1)
        .create_async()
        .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, error) = browser
        .request(
            "POST",
            &format!("{}/registrations", path(&tenant)),
            Some(json!({"namespace": ["db"], "table": "orders"})),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{error}");
    assert_eq!(error["reason"], "catalog_unavailable");
    assert_eq!(error["disposition"], "RetrySameRequest");
}
