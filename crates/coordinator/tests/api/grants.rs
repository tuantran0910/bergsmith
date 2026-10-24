use super::*;
use bergsmith_persistence::MIGRATOR;

/// Waits until a request in this test's database is blocked on this
/// organization's lifecycle lock.
async fn wait_for_administration_lock(
    pool: &PgPool,
    organization: bergsmith_common::ids::OrganizationId,
) -> bool {
    for _ in 0..500 {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                 SELECT 1
                 FROM pg_locks l
                 JOIN pg_database d ON d.oid = l.database
                 WHERE d.datname = current_database()
                   AND l.locktype = 'advisory'
                   AND l.objsubid = 2
                   AND l.classid = hashtext('bergsmith.principals')::oid
                   AND l.objid = hashtext($1::text)::oid
                   AND NOT l.granted)",
        )
        .bind(organization.to_string())
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_revocation_waits_for_the_lifecycle_lock(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let bob = support::user(&mut conn, tenant.organization, "bob").await;
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: bob,
            role: Role::Administrator,
            scope: GrantScope::organization(),
            granted_by: Some(tenant.user),
        },
    )
    .await
    .unwrap();
    let alice_grant: Uuid = sqlx::query_scalar(
        "SELECT id FROM principal_role_grants
         WHERE organization_id = $1 AND principal_id = $2 AND revoked_at IS NULL
           AND catalog_connection_id IS NULL AND table_identity_id IS NULL",
    )
    .bind(tenant.organization.as_uuid())
    .bind(tenant.user.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    // While the lifecycle lock is held, the other administrator loses their
    // access. The revocation must wait for the lock and re-read the
    // administration state, not act on what it saw before.
    let mut blocking = pool.begin().await.unwrap();
    tenancy::lock_principal_administration(&mut blocking, tenant.organization)
        .await
        .unwrap();
    let caller = browser.clone();
    let pending = tokio::spawn(async move {
        caller
            .request(
                "DELETE",
                &format!("/api/v1/grants/{alice_grant}"),
                None,
                None,
            )
            .await
    });
    assert!(
        wait_for_administration_lock(&pool, tenant.organization).await,
        "the revocation must wait for the organization lifecycle lock"
    );
    tenancy::deactivate_principal(&mut blocking, tenant.organization, bob)
        .await
        .unwrap();
    blocking.commit().await.unwrap();

    let (status, body) = pending.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "last_administrator");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn administrators_page_create_and_revoke_grants(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let bob = support::user(&mut conn, tenant.organization, "bob").await;
    let connection_grant = RoleGrantId::generate();
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: connection_grant,
            organization_id: tenant.organization,
            principal_id: bob,
            role: Role::Operator,
            scope: GrantScope::connection(tenant.connection),
            granted_by: Some(tenant.user),
        },
    )
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    // The organization administrator sees every active grant.
    let (status, page) = browser.request("GET", "/api/v1/grants", None, None).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(page["next_cursor"].is_null());
    let listed = items
        .iter()
        .find(|grant| grant["id"] == connection_grant.to_string())
        .expect("the connection grant is listed");
    assert_eq!(listed["principal_id"], bob.to_string());
    assert_eq!(listed["role"], "Operator");
    assert_eq!(
        listed["scope"]["catalog_connection"],
        tenant.connection.to_string()
    );
    assert!(listed["scope"]["table"].is_null());
    assert_eq!(listed["granted_by"], tenant.user.to_string());
    assert!(listed["granted_at"].is_string());

    // A principal filter narrows the page without widening the scope.
    let (status, page) = browser
        .request(
            "GET",
            &format!("/api/v1/grants?principal_id={bob}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);

    let (status, org_grant) = browser
        .request(
            "POST",
            "/api/v1/grants",
            Some(json!({
                "principal_id": bob,
                "role": "Viewer",
                "scope": {}
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{org_grant}");
    assert_eq!(org_grant["role"], "Viewer");
    assert!(org_grant["scope"]["catalog_connection"].is_null());
    assert!(org_grant["scope"]["table"].is_null());

    let (status, table_grant) = browser
        .request(
            "POST",
            "/api/v1/grants",
            Some(json!({
                "principal_id": bob,
                "role": "Viewer",
                "scope": {"table": tenant.table}
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{table_grant}");
    assert_eq!(table_grant["scope"]["table"], tenant.table.to_string());

    // An identical active grant is a conflict, not a duplicate row.
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/grants",
            Some(json!({
                "principal_id": bob,
                "role": "Viewer",
                "scope": {}
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "grant_already_exists");

    // A grant names one scope, and its role is a known one.
    for body in [
        json!({
            "principal_id": bob,
            "role": "Viewer",
            "scope": {"catalog_connection": tenant.connection, "table": tenant.table}
        }),
        json!({"principal_id": bob, "role": "Superuser"}),
        json!({"principal_id": Uuid::now_v7(), "role": "Viewer"}),
    ] {
        let (status, body) = browser
            .request("POST", "/api/v1/grants", Some(body), None)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    // The page is cursor-stable over three grants.
    let (status, first) = browser
        .request(
            "GET",
            &format!("/api/v1/grants?principal_id={bob}&limit=2"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let cursor = first["next_cursor"].as_str().expect("a second page");
    // The cursor is bound to the filters, so it cannot be reused on a page
    // that selects different grants.
    assert_eq!(
        browser
            .request(
                "GET",
                &format!(
                    "/api/v1/grants?principal_id={}&limit=2&cursor={cursor}",
                    tenant.user
                ),
                None,
                None
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let mut ids: Vec<String> = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|grant| grant["id"].as_str().unwrap().to_owned())
        .collect();
    let (status, second) = browser
        .request(
            "GET",
            &format!("/api/v1/grants?principal_id={bob}&limit=2&cursor={cursor}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert!(second["next_cursor"].is_null());
    ids.extend(
        second["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|grant| grant["id"].as_str().unwrap().to_owned()),
    );
    assert_eq!(ids.len(), 3);
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3, "the cursor never repeats a grant");

    let org_grant_id = org_grant["id"].as_str().unwrap();
    let (status, revoked) = browser
        .request(
            "DELETE",
            &format!("/api/v1/grants/{org_grant_id}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(revoked["id"], org_grant_id);
    assert_eq!(revoked["role"], "Viewer");
    // The row is kept for audit and a repeated revocation is a 404.
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM principal_role_grants
         WHERE id = $1 AND revoked_at IS NOT NULL AND revoked_by = $2",
    )
    .bind(org_grant["id"].as_str().unwrap().parse::<Uuid>().unwrap())
    .bind(tenant.user.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kept, 1);
    assert_eq!(
        browser
            .request(
                "DELETE",
                &format!("/api/v1/grants/{org_grant_id}"),
                None,
                None
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_events
         WHERE resource_id = $1 AND action IN ('grant.created', 'grant.revoked')
         ORDER BY id",
    )
    .bind(org_grant_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(actions, vec!["grant.created", "grant.revoked"]);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn grants_cannot_exceed_the_actors_scope(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other = support::connection(&mut conn, tenant.organization, "other").await;
    let bob = support::user(&mut conn, tenant.organization, "bob").await;
    let org_grant: Uuid = sqlx::query_scalar(
        "SELECT id FROM principal_role_grants
         WHERE organization_id = $1 AND principal_id = $2 AND revoked_at IS NULL
           AND catalog_connection_id IS NULL AND table_identity_id IS NULL",
    )
    .bind(tenant.organization.as_uuid())
    .bind(tenant.user.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let (browser, own_grant) = scoped(
        &pool,
        &tenant,
        Role::Administrator,
        GrantScope::connection(tenant.connection),
    )
    .await;

    // Reading the organization page or granting organization-wide access is
    // beyond a connection administrator.
    assert_eq!(
        browser.request("GET", "/api/v1/grants", None, None).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, _) = browser
        .request(
            "POST",
            "/api/v1/grants",
            Some(json!({"principal_id": bob, "role": "Viewer"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // So is another connection.
    let (status, _) = browser
        .request(
            "POST",
            "/api/v1/grants",
            Some(json!({
                "principal_id": bob,
                "role": "Viewer",
                "scope": {"catalog_connection": other}
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Within its own connection, including its tables, it may grant.
    let (status, connection_grant) = browser
        .request(
            "POST",
            "/api/v1/grants",
            Some(json!({
                "principal_id": bob,
                "role": "Viewer",
                "scope": {"catalog_connection": tenant.connection}
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{connection_grant}");
    let (status, table_grant) = browser
        .request(
            "POST",
            "/api/v1/grants",
            Some(json!({
                "principal_id": bob,
                "role": "Viewer",
                "scope": {"table": tenant.table}
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{table_grant}");

    // Its connection page carries the grants that apply to that connection,
    // including the table grant, and never the organization-wide ones.
    let (status, page) = browser
        .request(
            "GET",
            &format!("/api/v1/grants?catalog_connection={}", tenant.connection),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let ids: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|grant| grant["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&own_grant.to_string().as_str()), "{ids:?}");
    assert!(
        ids.contains(&connection_grant["id"].as_str().unwrap()),
        "{ids:?}"
    );
    assert!(
        ids.contains(&table_grant["id"].as_str().unwrap()),
        "{ids:?}"
    );
    assert!(!ids.contains(&org_grant.to_string().as_str()), "{ids:?}");

    // A table page is within the connection grant too.
    assert_eq!(
        browser
            .request(
                "GET",
                &format!("/api/v1/grants?table={}", tenant.table),
                None,
                None
            )
            .await
            .0,
        StatusCode::OK
    );

    // It cannot revoke above its scope, but it can revoke inside it.
    assert_eq!(
        browser
            .request("DELETE", &format!("/api/v1/grants/{org_grant}"), None, None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        browser
            .request(
                "DELETE",
                &format!(
                    "/api/v1/grants/{}",
                    connection_grant["id"].as_str().unwrap()
                ),
                None,
                None
            )
            .await
            .0,
        StatusCode::OK
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_last_administrator_grant_cannot_be_revoked(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let bob = support::user(&mut conn, tenant.organization, "bob").await;
    let alice_grant: Uuid = sqlx::query_scalar(
        "SELECT id FROM principal_role_grants
         WHERE organization_id = $1 AND principal_id = $2 AND revoked_at IS NULL
           AND catalog_connection_id IS NULL AND table_identity_id IS NULL",
    )
    .bind(tenant.organization.as_uuid())
    .bind(tenant.user.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    // A scoped Administrator grant does not replace an organization
    // administrator.
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: bob,
            role: Role::Administrator,
            scope: GrantScope::connection(tenant.connection),
            granted_by: Some(tenant.user),
        },
    )
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, body) = browser
        .request(
            "DELETE",
            &format!("/api/v1/grants/{alice_grant}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "last_administrator");

    // A second active organization administrator is a replacement.
    let mut conn = pool.acquire().await.unwrap();
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: bob,
            role: Role::Administrator,
            scope: GrantScope::organization(),
            granted_by: Some(tenant.user),
        },
    )
    .await
    .unwrap();
    drop(conn);
    assert_eq!(
        browser
            .request(
                "DELETE",
                &format!("/api/v1/grants/{alice_grant}"),
                None,
                None
            )
            .await
            .0,
        StatusCode::OK
    );
    let administrators: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM principal_role_grants g
         JOIN principals p
           ON p.organization_id = g.organization_id AND p.id = g.principal_id
         WHERE g.organization_id = $1 AND g.revoked_at IS NULL
           AND g.role = 'Administrator' AND g.catalog_connection_id IS NULL
           AND g.table_identity_id IS NULL AND p.deactivated_at IS NULL",
    )
    .bind(tenant.organization.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(administrators, 1);
}
