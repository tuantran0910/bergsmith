use super::*;

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn administrators_provision_list_and_deactivate_a_user(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, created) = browser
        .request(
            "POST",
            "/api/v1/principals",
            Some(json!({
                "issuer": "https://idp.example",
                "subject": "bob",
                "display_name": "Bob"
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["kind"], "User");
    assert_eq!(created["active"], true);
    assert!(created["deactivated_at"].is_null());
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, page) = browser
        .request("GET", "/api/v1/principals", None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert!(page["next_cursor"].is_null());

    let (status, detail) = browser
        .request("GET", &format!("/api/v1/principals/{id}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail, created);

    // The user can act before deactivation, so the later refusal is the
    // deactivation and not a missing grant.
    let bob = id.parse::<PrincipalId>().unwrap();
    let mut conn = pool.acquire().await.unwrap();
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: bob,
            role: Role::Viewer,
            scope: GrantScope::organization(),
            granted_by: Some(tenant.user),
        },
    )
    .await
    .unwrap();
    drop(conn);
    let user = Browser::new(&pool, &tenant, bob, "bob").await;
    assert_eq!(
        user.request("GET", "/api/v1/tables", None, None).await.0,
        StatusCode::OK
    );

    let (status, deactivated) = browser
        .request(
            "PATCH",
            &format!("/api/v1/principals/{id}"),
            Some(json!({ "active": false })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{deactivated}");
    assert_eq!(deactivated["active"], false);
    assert!(deactivated["deactivated_at"].is_string());
    // The row, the name, and the grant stay for audit.
    assert_eq!(deactivated["display_name"], "Bob");
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM principal_role_grants
         WHERE principal_id = $1 AND revoked_at IS NULL",
    )
    .bind(bob.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, 1);

    // Its sessions ended with it, and a fresh request is unattributed.
    assert_eq!(
        user.request("GET", "/api/v1/tables", None, None).await.0,
        StatusCode::UNAUTHORIZED
    );

    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_events
         WHERE resource_id = $1
           AND action IN ('identity.provisioned', 'principal.deactivated')
         ORDER BY id",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        actions,
        vec!["identity.provisioned", "principal.deactivated"]
    );

    // Reactivation restores the grant that was kept; the old session stays
    // revoked, so a new sign-in is required.
    let (status, reactivated) = browser
        .request(
            "PATCH",
            &format!("/api/v1/principals/{id}"),
            Some(json!({ "active": true })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reactivated}");
    assert_eq!(reactivated["active"], true);
    assert!(reactivated["deactivated_at"].is_null());
    let fresh = Browser::new(&pool, &tenant, bob, "bob").await;
    assert_eq!(
        fresh.request("GET", "/api/v1/tables", None, None).await.0,
        StatusCode::OK
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_deactivated_principal_cannot_keep_an_existing_session(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let dave = support::user(&mut conn, tenant.organization, "dave").await;
    drop(conn);
    // Deactivation through the ledger alone, without revoking sessions, so the
    // session lookup itself must notice.
    let session = Browser::new(&pool, &tenant, dave, "dave").await;
    let mut tx = pool.begin().await.unwrap();
    tenancy::deactivate_principal(&mut tx, tenant.organization, dave)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        session.request("GET", "/api/v1/tables", None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn principal_administration_requires_grants_manage(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let (browser, _) = scoped(&pool, &tenant, Role::Operator, GrantScope::organization()).await;
    let (status, body) = browser
        .request("GET", "/api/v1/principals", None, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["reason"], "permission_denied");
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/principals",
            Some(json!({
                "issuer": "https://idp.example",
                "subject": "bob",
                "display_name": "Bob"
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["reason"], "permission_denied");
    let (status, body) = browser
        .request(
            "PATCH",
            &format!("/api/v1/principals/{}", tenant.user),
            Some(json!({ "active": false })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["reason"], "permission_denied");
    let denials: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE action = 'api.permission_denied' AND detail->>'permission' = 'grants:manage'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(denials, 3);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_last_active_administrator_cannot_be_deactivated(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let carol_id = support::user(&mut conn, tenant.organization, "carol").await;
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: carol_id,
            role: Role::Administrator,
            scope: GrantScope::organization(),
            granted_by: Some(tenant.user),
        },
    )
    .await
    .unwrap();
    drop(conn);
    let alice = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let carol = Browser::new(&pool, &tenant, carol_id, "carol").await;

    // Two active administrators: either may step down.
    let (status, body) = alice
        .request(
            "PATCH",
            &format!("/api/v1/principals/{}", tenant.user),
            Some(json!({ "active": false })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Carol is now the last one.
    let (status, body) = carol
        .request(
            "PATCH",
            &format!("/api/v1/principals/{carol_id}"),
            Some(json!({ "active": false })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "last_administrator");

    let (status, body) = carol
        .request(
            "PATCH",
            &format!("/api/v1/principals/{}", tenant.user),
            Some(json!({ "active": true })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Alice holds the administrator grant again, so Carol may step down.
    let (status, body) = carol
        .request(
            "PATCH",
            &format!("/api/v1/principals/{carol_id}"),
            Some(json!({ "active": false })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn provisioning_an_existing_identity_conflicts(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let body = json!({
        "issuer": "https://idp.example",
        "subject": "alice",
        "display_name": "Alice"
    });
    let (status, created) = browser
        .request("POST", "/api/v1/principals", Some(body.clone()), None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{created}");
    assert_eq!(created["reason"], "resource_already_exists");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn unknown_and_malformed_principal_requests_are_rejected(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let unknown = PrincipalId::generate();
    for (method, uri, body, expected) in [
        (
            "GET",
            format!("/api/v1/principals/{unknown}"),
            None,
            StatusCode::NOT_FOUND,
        ),
        (
            "PATCH",
            format!("/api/v1/principals/{unknown}"),
            Some(json!({ "active": false })),
            StatusCode::NOT_FOUND,
        ),
        (
            "POST",
            "/api/v1/principals".to_owned(),
            Some(json!({"issuer": "https://idp.example", "subject": "", "display_name": "Bob"})),
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            "/api/v1/principals".to_owned(),
            Some(json!({
                "issuer": "https://idp.example",
                "subject": "bob",
                "display_name": "x".repeat(201)
            })),
            StatusCode::BAD_REQUEST,
        ),
        (
            "POST",
            "/api/v1/principals".to_owned(),
            Some(json!({"issuer": "https://idp.example", "subject": "bob"})),
            StatusCode::BAD_REQUEST,
        ),
        (
            "PATCH",
            format!("/api/v1/principals/{}", tenant.user),
            Some(json!({})),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let (status, response) = browser.request(method, &uri, body, None).await;
        assert_eq!(status, expected, "{method} {uri}: {response}");
    }
    let (status, body) = browser
        .request("GET", "/api/v1/principals?kind=Robot", None, None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = browser
        .request("GET", "/api/v1/principals?limit=0", None, None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn repeated_deactivation_changes_nothing_and_writes_no_audit(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let bob = support::user(&mut conn, tenant.organization, "bob").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let uri = format!("/api/v1/principals/{bob}");
    assert_eq!(
        browser
            .request("PATCH", &uri, Some(json!({ "active": false })), None)
            .await
            .0,
        StatusCode::OK
    );
    let (status, body) = browser
        .request("PATCH", &uri, Some(json!({ "active": false })), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["active"], false);
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE resource_id = $1 AND action = 'principal.deactivated'",
    )
    .bind(bob.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn principals_page_by_identifier_and_filter_by_kind(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    support::user(&mut conn, tenant.organization, "bob").await;
    support::user(&mut conn, tenant.organization, "carol").await;
    let scheduler = support::service_principal(&mut conn, tenant.organization, "scheduler").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..3 {
        let uri = match &cursor {
            Some(cursor) => format!("/api/v1/principals?limit=2&cursor={cursor}"),
            None => "/api/v1/principals?limit=2".to_owned(),
        };
        let (status, body) = browser.request("GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        seen.extend(
            body["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["id"].as_str().unwrap().to_owned()),
        );
        match body["next_cursor"].as_str() {
            Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
            _ => break,
        }
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 4, "every principal is reachable by paging");

    let (status, body) = browser
        .request("GET", "/api/v1/principals?kind=Service", None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], scheduler.to_string());
    assert_eq!(items[0]["kind"], "Service");
}
