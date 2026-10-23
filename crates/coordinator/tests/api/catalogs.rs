use super::*;
use std::sync::Arc;

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn legacy_settings_do_not_prevent_disabling_or_renaming(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/catalogs/{}", tenant.connection);
    for (index, uri) in [
        "https://user:private-password@catalog.example/api?token=private-token#private-fragment",
        "https://catalog.example/legacy path",
        "https://[invalid",
    ]
    .iter()
    .enumerate()
    {
        sqlx::query("UPDATE catalog_connections SET uri=$2, warehouse='', credential_ref='legacy ref' WHERE id=$1")
            .bind(tenant.connection.as_uuid()).bind(uri).execute(&mut *conn).await.unwrap();
        let revision = i64::try_from(index).unwrap() * 2;
        let (status, updated) = browser
            .request(
                "PATCH",
                &path,
                Some(json!({"expected_revision":revision,"enabled":false})),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{updated}");
        let (status, updated) = browser
            .request(
                "PATCH",
                &path,
                Some(json!({"expected_revision":revision+1,"name":format!("renamed-{index}")})),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{updated}");
        if index == 2 {
            assert!(updated.get("uri").is_none());
        }
        let stored = tenancy::catalog_connection(&mut conn, tenant.organization, tenant.connection)
            .await
            .unwrap();
        assert_eq!(&stored.uri, uri);
        assert_eq!(stored.warehouse.as_deref(), Some(""));
        assert_eq!(stored.credential_ref.as_deref(), Some("legacy ref"));
        assert!(!stored.enabled);
    }
    assert_eq!(
        bergsmith_persistence::jobs::job_by_id(&mut conn, job)
            .await
            .unwrap()
            .state,
        JobState::Planning
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn empty_catalog_patches_leave_revision_and_audit_unchanged(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/catalogs/{}", tenant.connection);
    assert_eq!(
        browser
            .request("PATCH", &path, Some(json!({"expected_revision":0})), None)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        browser.request("GET", &path, None, None).await.1["revision"],
        0
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='catalog.updated'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn creating_a_catalog_returns_its_detail_location(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let response = browser
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/catalogs")
                .header("cookie", &browser.cookie)
                .header(CSRF_HEADER, &browser.csrf)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"name":"new","uri":"https://catalog.example/api"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response.headers()["location"].to_str().unwrap().to_owned();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(
        location,
        format!("/api/v1/catalogs/{}", body["id"].as_str().unwrap())
    );
    assert_eq!(browser.request("GET", &location, None, None).await.1, body);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_capability_check_records_what_the_catalog_advertises(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let mut server = mockito::Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_body(
            r#"{"defaults":{},"overrides":{},"endpoints":[
                "GET /v1/{prefix}/namespaces/{namespace}/tables",
                "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}",
                "POST /v1/{prefix}/namespaces/{namespace}/tables/{table}",
                "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}/credentials"],
                "idempotency-key-lifetime":"PT30M"}"#,
        )
        .expect(1)
        .create_async()
        .await;
    sqlx::query(
        "UPDATE catalog_connections SET uri=$2, warehouse=NULL, credential_ref=NULL WHERE id=$1",
    )
    .bind(tenant.connection.as_uuid())
    .bind(server.url())
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/catalogs/{}/check", tenant.connection);

    let (status, body) = browser.request("POST", &path, None, None).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["catalog_id"], tenant.connection.to_string());
    assert!(body["checked_at"].is_string());
    assert_eq!(body["capabilities"]["list_tables"], "met");
    assert_eq!(body["capabilities"]["load_table"], "met");
    assert_eq!(body["capabilities"]["commit_table"], "met");
    assert_eq!(body["capabilities"]["credentials_endpoint"], "met");
    // The configuration cannot show vended credentials; a table load proves them.
    assert_eq!(body["capabilities"]["vended_credentials"], "unknown");
    assert_eq!(
        body["capabilities"]["idempotency_key_lifetime_seconds"],
        "1800"
    );

    // The answer is recorded and shown with the connection's settings.
    let detail = browser
        .request(
            "GET",
            &format!("/api/v1/catalogs/{}", tenant.connection),
            None,
            None,
        )
        .await
        .1;
    assert_eq!(detail["capabilities"], body["capabilities"]);
    assert_eq!(detail["capabilities_checked_at"], body["checked_at"]);
    // A check is diagnostic: it never moves the configuration revision.
    assert_eq!(detail["revision"], 0);

    let records: Vec<(String, Value)> =
        sqlx::query_as("SELECT action, detail FROM audit_events WHERE resource_id=$1 ORDER BY id")
            .bind(tenant.connection.to_string())
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, "catalog.checked");
    assert_eq!(records[0].1["complete"], true);
    assert!(records[0].1["missing"].as_array().unwrap().is_empty());
    config.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_capability_check_reports_a_catalog_that_lacks_an_endpoint(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let mut server = mockito::Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_body(
            r#"{"defaults":{},"overrides":{},"endpoints":[
                "GET /v1/{prefix}/namespaces/{namespace}/tables"]}"#,
        )
        .expect(1)
        .create_async()
        .await;
    sqlx::query(
        "UPDATE catalog_connections SET uri=$2, warehouse=NULL, credential_ref=NULL WHERE id=$1",
    )
    .bind(tenant.connection.as_uuid())
    .bind(server.url())
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/catalogs/{}/check", tenant.connection);

    let (status, body) = browser.request("POST", &path, None, None).await;

    // A catalog that lacks an endpoint is reported, not refused.
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["capabilities"]["list_tables"], "met");
    assert_eq!(body["capabilities"]["load_table"], "missing");
    assert_eq!(body["capabilities"]["commit_table"], "missing");
    let records: Vec<(String, Value)> =
        sqlx::query_as("SELECT action, detail FROM audit_events WHERE resource_id=$1 ORDER BY id")
            .bind(tenant.connection.to_string())
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(records[0].1["complete"], false);
    assert_eq!(
        records[0].1["missing"],
        json!(["load_table", "commit_table", "credentials_endpoint"])
    );
    config.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_capability_check_needs_permission_and_a_reachable_catalog(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other = support::tenant(&mut conn, "other").await;
    let mut server = mockito::Server::new_async().await;
    let unavailable = server
        .mock("GET", "/v1/config")
        .with_status(500)
        .expect(1)
        .create_async()
        .await;
    sqlx::query(
        "UPDATE catalog_connections SET uri=$2, warehouse=NULL, credential_ref=NULL WHERE id=$1",
    )
    .bind(tenant.connection.as_uuid())
    .bind(server.url())
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let path = format!("/api/v1/catalogs/{}/check", tenant.connection);

    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::connection(tenant.connection),
    )
    .await;
    assert_eq!(
        viewer.request("POST", &path, None, None).await.0,
        StatusCode::FORBIDDEN
    );

    let (admin, _) = scoped(
        &pool,
        &tenant,
        Role::Administrator,
        GrantScope::connection(tenant.connection),
    )
    .await;
    let (status, body) = admin.request("POST", &path, None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["reason"], "catalog_unavailable");

    // A failed check records nothing.
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        bergsmith_persistence::capabilities::latest_check(
            &mut conn,
            tenant.organization,
            tenant.connection
        )
        .await
        .unwrap()
        .is_none()
    );
    drop(conn);

    // Another organization's connection is not reachable from this one.
    let foreign = format!("/api/v1/catalogs/{}/check", other.connection);
    assert_eq!(
        admin.request("POST", &foreign, None, None).await.0,
        StatusCode::FORBIDDEN
    );
    unavailable.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_capability_check_of_a_credentialed_connection_is_refused(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/catalogs/{}/check", tenant.connection);

    // The connection names an external credential the coordinator cannot
    // resolve, so there is nothing to check.
    let (status, body) = browser.request("POST", &path, None, None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["reason"], "catalog_credential_unavailable");
}

/// A catalog that answers only when the test releases it.
///
/// The channel reports that a request arrived; the semaphore releases one
/// waiting answer at a time. No test depends on a timer, so a slow machine
/// cannot change the outcome.
async fn slow_catalog() -> (
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
            let _ = arrived.send(());
            let release = shared.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer).await;
                // The test decides when the catalog answers.
                let _ = release.acquire().await;
                let body = CONFIG_BODY;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://{address}"), arrived_rx, release)
}

const CONFIG_BODY: &str = r#"{"defaults":{},"overrides":{},"endpoints":[
    "GET /v1/{prefix}/namespaces/{namespace}/tables",
    "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}",
    "POST /v1/{prefix}/namespaces/{namespace}/tables/{table}"]}"#;

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_capability_check_is_not_recorded_when_the_connection_changes(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (url, mut arrived, release) = slow_catalog().await;
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
    let path = format!("/api/v1/catalogs/{}/check", tenant.connection);

    // The check is waiting for the catalog when the connection is edited.
    let checking = {
        let browser = browser.clone();
        let path = path.clone();
        tokio::spawn(async move { browser.request("POST", &path, None, None).await })
    };
    arrived.recv().await.expect("the catalog was asked");
    let (status, edited) = browser
        .request(
            "PATCH",
            &format!("/api/v1/catalogs/{}", tenant.connection),
            Some(json!({"expected_revision":0,"uri":"https://other.example/catalog"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{edited}");

    // The catalog answers only now, after the connection changed.
    release.add_permits(1);
    let (status, body) = checking.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "catalog_connection_changed");

    // The answer describes a connection that no longer exists, so nothing was
    // recorded.
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        bergsmith_persistence::capabilities::latest_check(
            &mut conn,
            tenant.organization,
            tenant.connection
        )
        .await
        .unwrap()
        .is_none()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_recorded_capability_answer_is_suppressed_after_a_routing_edit(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let mut server = mockito::Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_body(CONFIG_BODY)
        .expect(1)
        .create_async()
        .await;
    sqlx::query(
        "UPDATE catalog_connections SET uri=$2, warehouse=NULL, credential_ref=NULL WHERE id=$1",
    )
    .bind(tenant.connection.as_uuid())
    .bind(server.url())
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, _) = browser
        .request(
            "POST",
            &format!("/api/v1/catalogs/{}/check", tenant.connection),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let detail = browser
        .request(
            "GET",
            &format!("/api/v1/catalogs/{}", tenant.connection),
            None,
            None,
        )
        .await
        .1;
    assert_eq!(detail["capabilities"]["list_tables"], "met");

    // A routing edit makes the recorded answer stale, so it is not shown.
    let (status, edited) = browser
        .request(
            "PATCH",
            &format!("/api/v1/catalogs/{}", tenant.connection),
            Some(json!({"expected_revision":0,"uri":"https://other.example/catalog"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    let detail = browser
        .request(
            "GET",
            &format!("/api/v1/catalogs/{}", tenant.connection),
            None,
            None,
        )
        .await
        .1;
    assert!(detail.get("capabilities").is_none());
    assert!(detail.get("capabilities_checked_at").is_none());
    config.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn clearing_an_empty_setting_suppresses_the_recorded_answer(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let mut server = mockito::Server::new_async().await;
    let config = server
        // The connection has an empty warehouse, so the catalog is asked with
        // an empty warehouse parameter.
        .mock("GET", "/v1/config?warehouse=")
        .with_body(CONFIG_BODY)
        .expect(1)
        .create_async()
        .await;
    // A legacy row with an empty warehouse: the catalog is asked without a
    // warehouse parameter, which is not the same request as an empty one.
    sqlx::query(
        "UPDATE catalog_connections SET uri=$2, warehouse='', credential_ref=NULL WHERE id=$1",
    )
    .bind(tenant.connection.as_uuid())
    .bind(server.url())
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, body) = browser
        .request(
            "POST",
            &format!("/api/v1/catalogs/{}/check", tenant.connection),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let detail = browser
        .request(
            "GET",
            &format!("/api/v1/catalogs/{}", tenant.connection),
            None,
            None,
        )
        .await
        .1;
    assert_eq!(detail["capabilities"]["list_tables"], "met");

    // Clearing the warehouse is a different setting from an empty one, so
    // the recorded answer no longer applies.
    let (status, edited) = browser
        .request(
            "PATCH",
            &format!("/api/v1/catalogs/{}", tenant.connection),
            Some(json!({"expected_revision":0,"warehouse":null})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{edited}");
    let detail = browser
        .request(
            "GET",
            &format!("/api/v1/catalogs/{}", tenant.connection),
            None,
            None,
        )
        .await
        .1;
    assert!(detail.get("capabilities").is_none());
    config.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_capability_check_refuses_an_oversized_catalog_answer(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let mut server = mockito::Server::new_async().await;
    let oversized = server
        .mock("GET", "/v1/config")
        .with_body("x".repeat(1024 * 1024))
        .expect(1)
        .create_async()
        .await;
    sqlx::query(
        "UPDATE catalog_connections SET uri=$2, warehouse=NULL, credential_ref=NULL WHERE id=$1",
    )
    .bind(tenant.connection.as_uuid())
    .bind(server.url())
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, body) = browser
        .request(
            "POST",
            &format!("/api/v1/catalogs/{}/check", tenant.connection),
            None,
            None,
        )
        .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["reason"], "catalog_response_too_large");
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        bergsmith_persistence::capabilities::latest_check(
            &mut conn,
            tenant.organization,
            tenant.connection
        )
        .await
        .unwrap()
        .is_none()
    );
    oversized.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_capability_check_is_refused_when_the_requester_is_at_its_bound(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (url, mut arrived, release) = slow_catalog().await;
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
    let path = format!("/api/v1/catalogs/{}/check", tenant.connection);

    // Two checks for one requester run at once; the third is refused rather
    // than queued.
    let mut running = Vec::new();
    for _ in 0..2 {
        let browser = browser.clone();
        let path = path.clone();
        running.push(tokio::spawn(async move {
            browser.request("POST", &path, None, None).await
        }));
    }
    arrived.recv().await.expect("the catalog was asked");
    arrived.recv().await.expect("the catalog was asked");

    let (status, body) = browser.request("POST", &path, None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["reason"], "capability_check_capacity_exhausted");
    release.add_permits(2);
    for check in running {
        let (status, body) = check.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_capability_check_keeps_the_advertised_lifetime_exact(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let mut server = mockito::Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_body(
            r#"{"defaults":{},"overrides":{},"idempotency-key-lifetime":"PT9007199254740993S"}"#,
        )
        .expect(1)
        .create_async()
        .await;
    sqlx::query(
        "UPDATE catalog_connections SET uri=$2, warehouse=NULL, credential_ref=NULL WHERE id=$1",
    )
    .bind(tenant.connection.as_uuid())
    .bind(server.url())
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, body) = browser
        .request(
            "POST",
            &format!("/api/v1/catalogs/{}/check", tenant.connection),
            None,
            None,
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    // A lifetime beyond what a browser counts exactly crosses as a decimal
    // string, unchanged.
    assert_eq!(
        body["capabilities"]["idempotency_key_lifetime_seconds"],
        "9007199254740993"
    );
    config.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn catalog_details_do_not_expose_embedded_authentication_in_legacy_rows(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    sqlx::query("UPDATE catalog_connections SET uri='https://user:private-password@catalog.example/api?token=private-token#private-fragment' WHERE id=$1")
        .bind(tenant.connection.as_uuid()).execute(&mut *conn).await.unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, detail) = browser
        .request(
            "GET",
            &format!("/api/v1/catalogs/{}", tenant.connection),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["uri"], "https://catalog.example/api");
    assert!(!detail.to_string().contains("private-"));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn catalog_commands_are_revision_checked_and_audited(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let request = json!({"name":"warehouse","uri":"https://catalog.example/v1",
        "warehouse":"analytics","credential_ref":"secret/catalog"});
    let (status, created) = browser
        .request("POST", "/api/v1/catalogs", Some(request.clone()), None)
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(created["revision"], 0);
    assert_eq!(created["enabled"], true);
    let path = format!("/api/v1/catalogs/{}", created["id"].as_str().unwrap());
    assert_eq!(browser.request("GET", &path, None, None).await.1, created);
    assert_eq!(
        browser
            .request("POST", "/api/v1/catalogs", Some(request), None)
            .await
            .0,
        StatusCode::CONFLICT
    );
    let (status, updated) = browser
        .request(
            "PATCH",
            &path,
            Some(
                json!({"expected_revision":0,"name":"renamed","uri":"https://new.example/catalog",
            "warehouse":null,"credential_ref":null,"enabled":false}),
            ),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["revision"], 1);
    assert_eq!(updated["name"], "renamed");
    assert_eq!(updated["enabled"], false);
    assert!(updated["warehouse"].is_null());
    assert!(updated["credential_ref"].is_null());
    let (status, error) = browser
        .request(
            "PATCH",
            &path,
            Some(json!({"expected_revision":0,"enabled":true})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["reason"], "revision_conflict");
    let updated = browser
        .request(
            "PATCH",
            &path,
            Some(json!({"expected_revision":1,"enabled":true})),
            None,
        )
        .await
        .1;
    assert_eq!(updated["uri"], "https://new.example/catalog");
    let records: Vec<(String, Value)> =
        sqlx::query_as("SELECT action, detail FROM audit_events WHERE resource_id=$1 ORDER BY id")
            .bind(created["id"].as_str().unwrap())
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        records.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        ["catalog.created", "catalog.updated", "catalog.updated"]
    );
    assert!(!format!("{records:?}").contains("secret/catalog"));
    assert!(!format!("{records:?}").contains("new.example"));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn catalog_commands_respect_scope_and_keep_configuration_private(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other = support::tenant(&mut conn, "other").await;
    drop(conn);
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::connection(tenant.connection),
    )
    .await;
    let path = format!("/api/v1/catalogs/{}", tenant.connection);
    let (status, detail) = viewer.request("GET", &path, None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.get("uri").is_none());
    assert!(detail.get("credential_ref").is_none());
    assert_eq!(
        viewer
            .request(
                "PATCH",
                &path,
                Some(json!({"expected_revision":0,"enabled":false})),
                None
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (admin, _) = scoped(
        &pool,
        &tenant,
        Role::Administrator,
        GrantScope::connection(tenant.connection),
    )
    .await;
    assert_eq!(
        admin
            .request(
                "PATCH",
                &path,
                Some(json!({"expected_revision":0,"enabled":false})),
                None
            )
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        admin
            .request(
                "POST",
                "/api/v1/catalogs",
                Some(json!({"name":"new","uri":"https://catalog.example"})),
                None
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let foreign = format!("/api/v1/catalogs/{}", other.connection);
    assert_eq!(
        admin
            .request(
                "PATCH",
                &foreign,
                Some(json!({"expected_revision":0,"enabled":false})),
                None
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        admin.request("GET", &foreign, None, None).await.0,
        StatusCode::NOT_FOUND
    );
    let denials: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action='api.permission_denied' AND detail->>'permission'='connections:manage'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(
        denials, 3,
        "denial audits survive the authorization rollback"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn catalog_commands_reject_embedded_credentials_and_unknown_fields(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    for body in [
        json!({"name":"valid","uri":"https://user:password@catalog.example"}),
        json!({"name":"valid","uri":"https://catalog.example?token=secret"}),
        json!({"name":"valid","uri":"https://catalog.example#secret"}),
        json!({"name":"valid","uri":"file:///tmp/catalog"}),
        json!({"name":"invalid name","uri":"https://catalog.example"}),
        json!({"name":"valid","uri":"https://catalog.example","credential_ref":"token=secret"}),
        json!({"name":"valid","uri":"https://catalog.example","token":"secret"}),
    ] {
        assert_eq!(
            browser
                .request("POST", "/api/v1/catalogs", Some(body), None)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn catalog_changes_roll_back_when_audit_fails(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    sqlx::query("CREATE FUNCTION reject_catalog_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test audit unavailable'; END $$").execute(&mut *conn).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_catalog_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_catalog_audit()").execute(&mut *conn).await.unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/catalogs/{}", tenant.connection);
    assert_eq!(
        browser
            .request(
                "PATCH",
                &path,
                Some(json!({"expected_revision":0,"enabled":false})),
                None
            )
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        browser.request("GET", &path, None, None).await.1["enabled"],
        true
    );
    assert_eq!(
        browser
            .request(
                "POST",
                "/api/v1/catalogs",
                Some(json!({"name":"new","uri":"https://catalog.example"})),
                None
            )
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM catalog_connections")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn disabled_connections_refuse_previews_and_preserve_recovery_settings(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, job, JobState::CommitOutcomeUnknown).await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/catalogs/{}", tenant.connection);
    assert_eq!(
        browser
            .request(
                "PATCH",
                &path,
                Some(json!({"expected_revision":0,"enabled":false})),
                None
            )
            .await
            .0,
        StatusCode::OK
    );
    let (status, error) = browser
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id":tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["reason"], "catalog_connection_disabled");
    for change in [
        json!({"uri":"https://replacement.example"}),
        json!({"warehouse":"replacement"}),
        json!({"credential_ref":"replacement"}),
    ] {
        let mut body = change;
        body["expected_revision"] = json!(1);
        let (status, error) = browser.request("PATCH", &path, Some(body), None).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error["reason"], "catalog_connection_in_use");
    }
    let (status, renamed) = browser
        .request(
            "PATCH",
            &path,
            Some(json!({"expected_revision":1,"name":"renamed"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(renamed["name"], "renamed");
    let mut conn = pool.acquire().await.unwrap();
    let connection = tenancy::catalog_connection(&mut conn, tenant.organization, tenant.connection)
        .await
        .unwrap();
    assert_eq!(connection.uri, "https://catalog.example/catalog");
    assert_eq!(
        bergsmith_persistence::jobs::job_by_id(&mut conn, job)
            .await
            .unwrap()
            .state,
        JobState::CommitOutcomeUnknown
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_connection_keeps_the_bounds_it_is_given(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/catalogs/{}", tenant.connection);

    // A new connection starts with the default bounds and maintenance allowed.
    let (status, detail) = browser.request("GET", &path, None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["settings"]["maintenance_enabled"], true);
    assert_eq!(detail["settings"]["max_execution_seconds"], 180);
    assert_eq!(
        detail["settings"]["max_unresolved_publication_seconds"],
        3600
    );

    let (status, updated) = browser
        .request(
            "PATCH",
            &path,
            Some(json!({
                "expected_revision": 0,
                "max_execution_seconds": 600,
                "max_unresolved_publication_seconds": 7200,
                "maintenance_enabled": false,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["settings"]["maintenance_enabled"], false);
    assert_eq!(updated["settings"]["max_execution_seconds"], 600);
    assert_eq!(
        updated["settings"]["max_unresolved_publication_seconds"],
        7200
    );
    assert_eq!(updated["revision"], 1);

    let mut conn = pool.acquire().await.unwrap();
    let stored = tenancy::catalog_connection(&mut conn, tenant.organization, tenant.connection)
        .await
        .unwrap();
    assert_eq!(stored.settings.max_execution_seconds(), 600);
    assert_eq!(stored.settings.max_unresolved_publication_seconds(), 7200);
    assert!(!stored.settings.maintenance_enabled());

    // The audit names the bounds, never the connection's routing.
    let detail: Value = sqlx::query_scalar(
        "SELECT detail FROM audit_events WHERE action='catalog.updated' ORDER BY occurred_at DESC LIMIT 1",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(detail["maintenance_enabled"], false);
    assert_eq!(detail["max_execution_seconds"], 600);
    assert_eq!(detail["max_unresolved_publication_seconds"], 7200);
    assert!(detail.get("uri").is_none());
    assert!(detail.get("credential_ref").is_none());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn bounds_outside_their_range_are_refused(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/catalogs/{}", tenant.connection);
    for body in [
        json!({"expected_revision": 0, "max_execution_seconds": 0}),
        json!({"expected_revision": 0, "max_execution_seconds": 86401}),
        json!({"expected_revision": 0, "max_unresolved_publication_seconds": 0}),
        json!({"expected_revision": 0, "max_unresolved_publication_seconds": 2_592_001}),
    ] {
        let (status, error) = browser.request("PATCH", &path, Some(body), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    }
    // A refused bound leaves the stored bounds untouched.
    let mut conn = pool.acquire().await.unwrap();
    let stored = tenancy::catalog_connection(&mut conn, tenant.organization, tenant.connection)
        .await
        .unwrap();
    assert_eq!(stored.settings.max_execution_seconds(), 180);
    assert_eq!(stored.settings.max_unresolved_publication_seconds(), 3600);
    assert_eq!(stored.revision, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_detail_reports_the_effective_profile_and_the_unresolved_publications(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/catalogs/{}", tenant.connection);

    let (status, detail) = browser.request("GET", &path, None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    // The profiles stay in the coordinator configuration, so the connection
    // without an entry of its own uses the default set.
    assert_eq!(detail["storage_profile"]["source"], "default");
    assert_eq!(detail["storage_profile"]["backends"][0]["kind"], "local_fs");
    assert_eq!(detail["unresolved_publications"]["count"], 0);
    assert_eq!(detail["unresolved_publications"]["oldest_seconds"], 0);
    assert_eq!(detail["unresolved_publications"]["overdue"], false);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_summary_carries_the_same_bounds_as_the_detail(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, page) = browser.request("GET", "/api/v1/catalogs", None, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let summary = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == json!(tenant.connection.to_string()))
        .unwrap();
    assert_eq!(summary["settings"]["maintenance_enabled"], true);
    assert_eq!(summary["settings"]["max_execution_seconds"], 180);
    assert_eq!(
        summary["settings"]["max_unresolved_publication_seconds"],
        3600
    );
    // The summary never carries the connection's routing.
    assert!(summary.get("uri").is_none());
}

/// A previewer whose profiles name one connection of its own.
fn profiled_previewer(
    connection: CatalogConnectionId,
    profiles: StorageProfiles,
) -> CatalogPreviewer {
    CatalogPreviewer::new(
        PreviewProfiles::new(StorageProfiles::new([StorageProfile::LocalFs]))
            .with_connection(connection, profiles),
        PlannerConfig::default(),
    )
    .unwrap()
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_connection_with_its_own_profile_reports_it(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::with_previews(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        profiled_previewer(
            tenant.connection,
            StorageProfiles::new([StorageProfile::S3(S3Profile {
                endpoint: Some("http://127.0.0.1:59000".to_owned()),
                region: "us-east-1".to_owned(),
                path_style_access: true,
            })]),
        ),
    )
    .await;
    let path = format!("/api/v1/catalogs/{}", tenant.connection);

    let (status, detail) = browser.request("GET", &path, None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["storage_profile"]["source"], "connection");
    let backend = &detail["storage_profile"]["backends"][0];
    assert_eq!(backend["kind"], "s3");
    assert_eq!(backend["endpoint"], "http://127.0.0.1:59000/");
    assert_eq!(backend["region"], "us-east-1");
    assert_eq!(backend["path_style_access"], true);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_profile_without_optional_values_reports_them_as_absent(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    // A plain AWS profile names no endpoint, and a GCS profile may name no
    // project. Both are reported as absent, not as a missing field.
    let browser = Browser::with_previews(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        profiled_previewer(
            tenant.connection,
            StorageProfiles::new([
                StorageProfile::S3(S3Profile {
                    endpoint: None,
                    region: "us-east-1".to_owned(),
                    path_style_access: false,
                }),
                StorageProfile::Gcs(GcsProfile {
                    project: None,
                    endpoint: None,
                }),
            ]),
        ),
    )
    .await;
    let path = format!("/api/v1/catalogs/{}", tenant.connection);

    // The response is checked against the published contract.
    let (status, detail) = browser.request("GET", &path, None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let backends = detail["storage_profile"]["backends"]
        .as_array()
        .unwrap()
        .clone();
    let s3 = backends
        .iter()
        .find(|backend| backend["kind"] == "s3")
        .unwrap();
    assert_eq!(s3["endpoint"], json!(null));
    assert_eq!(s3["region"], "us-east-1");
    let gcs = backends
        .iter()
        .find(|backend| backend["kind"] == "gcs")
        .unwrap();
    assert_eq!(gcs["endpoint"], json!(null));
    assert_eq!(gcs["project"], json!(null));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_endpoint_is_reported_without_anything_secret(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::with_previews(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        profiled_previewer(
            tenant.connection,
            StorageProfiles::new([StorageProfile::S3(S3Profile {
                endpoint: Some(
                    "http://user:private-password@127.0.0.1:59000/data?token=private#fragment"
                        .to_owned(),
                ),
                region: "us-east-1".to_owned(),
                path_style_access: true,
            })]),
        ),
    )
    .await;
    let path = format!("/api/v1/catalogs/{}", tenant.connection);

    let (status, detail) = browser.request("GET", &path, None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let endpoint = detail["storage_profile"]["backends"][0]["endpoint"]
        .as_str()
        .unwrap();
    assert_eq!(endpoint, "http://127.0.0.1:59000/data");
    assert!(!detail.to_string().contains("private"));
}
