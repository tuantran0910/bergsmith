use super::*;

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
