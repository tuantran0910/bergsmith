//! Notification rules (BS-029.T2).

use super::*;
use bergsmith_common::ids::{CatalogConnectionId, NotificationDestinationId, TableId};
use bergsmith_common::notifications::AllowList;
use bergsmith_coordinator::notifications::{DeliveryClient, Settings};
use bergsmith_persistence::MIGRATOR;
use bergsmith_persistence::grants::{self, NewRoleGrant};

fn delivery() -> DeliveryClient {
    let settings = Settings::new(
        AllowList::new(Vec::<String>::new()).unwrap(),
        None,
        Duration::from_secs(5),
    );
    DeliveryClient::new(&settings).unwrap()
}

async fn destination(
    pool: &PgPool,
    tenant: &support::Tenant,
    name: &str,
) -> NotificationDestinationId {
    let id = NotificationDestinationId::generate();
    sqlx::query(
        "INSERT INTO notification_destinations
             (id, organization_id, name, kind, url_ref, created_by)
         VALUES ($1, $2, $3, 'slack', 'slack-url', $4)",
    )
    .bind(id.as_uuid())
    .bind(tenant.organization.as_uuid())
    .bind(name)
    .bind(tenant.user.as_uuid())
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn connection(pool: &PgPool, tenant: &support::Tenant, name: &str) -> CatalogConnectionId {
    let id = CatalogConnectionId::generate();
    sqlx::query(
        "INSERT INTO catalog_connections (id, organization_id, name, uri)
         VALUES ($1, $2, $3, 'https://catalog.example')",
    )
    .bind(id.as_uuid())
    .bind(tenant.organization.as_uuid())
    .bind(name)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn table(pool: &PgPool, tenant: &support::Tenant) -> TableId {
    let id = TableId::generate();
    sqlx::query(
        "INSERT INTO table_identities (id, organization_id, table_uuid)
         VALUES ($1, $2, $3)",
    )
    .bind(id.as_uuid())
    .bind(tenant.organization.as_uuid())
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .unwrap();
    id
}

fn rule_body(name: &str, destination: NotificationDestinationId) -> Value {
    json!({
        "name": name,
        "destination_id": destination,
        "event_kind": "job.replan_required",
        "filter": {"pool": "default"},
        "repeat_threshold": 3,
    })
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_rule_is_created_listed_and_edited(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let destination = destination(&pool, &tenant, "alerts").await;
    let browser =
        Browser::with_notifications(&pool, &tenant, tenant.user, "alice", delivery()).await;

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/rules",
            Some(rule_body("replans", destination)),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], "replans");
    assert_eq!(body["destination_id"], destination.to_string());
    assert_eq!(body["event_kind"], "job.replan_required");
    assert_eq!(body["filter"]["pool"], "default");
    assert_eq!(body["repeat_threshold"], 3);
    assert_eq!(body["enabled"], true);
    assert_eq!(body["revision"], 1);
    let id = body["id"].as_str().unwrap();

    let (status, body) = browser
        .request("GET", "/api/v1/notifications/rules", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], id);

    // Clearing the filter and the threshold is distinct from leaving them.
    let (status, body) = browser
        .request(
            "PATCH",
            &format!("/api/v1/notifications/rules/{id}"),
            Some(json!({
                "revision": 1,
                "name": "replans renamed",
                "filter": null,
                "repeat_threshold": null,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "replans renamed");
    assert_eq!(body["filter"], Value::Null);
    assert_eq!(body["repeat_threshold"], Value::Null);
    assert_eq!(body["revision"], 2);

    let (status, body) = browser
        .request(
            "PATCH",
            &format!("/api/v1/notifications/rules/{id}"),
            Some(json!({"revision": 1, "enabled": false})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "revision_conflict");

    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_events WHERE organization_id = $1 ORDER BY id",
    )
    .bind(tenant.organization.as_uuid())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        actions
            .iter()
            .any(|action| action == "notification.rule_created")
    );
    assert!(
        actions
            .iter()
            .any(|action| action == "notification.rule_updated")
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_event_kind_bounds_the_repeat_threshold(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let destination = destination(&pool, &tenant, "alerts").await;
    let browser =
        Browser::with_notifications(&pool, &tenant, tenant.user, "alice", delivery()).await;

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/rules",
            Some(json!({
                "name": "failures",
                "destination_id": destination,
                "event_kind": "job.failed",
                "repeat_threshold": 2,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/rules",
            Some(json!({
                "name": "unknown",
                "destination_id": destination,
                "event_kind": "job.exploded",
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/rules",
            Some(json!({
                "name": "replans",
                "destination_id": destination,
                "event_kind": "job.replan_required",
                "repeat_threshold": 2,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["repeat_threshold"], 2);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn filters_are_exactly_one_and_organization_scoped(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let destination = destination(&pool, &tenant, "alerts").await;
    let browser =
        Browser::with_notifications(&pool, &tenant, tenant.user, "alice", delivery()).await;

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/rules",
            Some(json!({
                "name": "two filters",
                "destination_id": destination,
                "event_kind": "job.failed",
                "filter": {"connection_id": Uuid::now_v7(), "pool": "default"},
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    for pool_name in ["Upper", "-leading", "has space"] {
        let (status, body) = browser
            .request(
                "POST",
                "/api/v1/notifications/rules",
                Some(json!({
                    "name": "bad pool",
                    "destination_id": destination,
                    "event_kind": "job.failed",
                    "filter": {"pool": pool_name},
                })),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{pool_name}: {body}");
    }

    // A filter target that is not in the organization is not found.
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/rules",
            Some(json!({
                "name": "missing connection",
                "destination_id": destination,
                "event_kind": "job.failed",
                "filter": {"connection_id": Uuid::now_v7()},
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["reason"], "resource_not_found");

    let mut conn = pool.acquire().await.unwrap();
    let other = support::tenant(&mut conn, "other").await;
    drop(conn);
    let foreign = table(&pool, &other).await;
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/rules",
            Some(json!({
                "name": "foreign table",
                "destination_id": destination,
                "event_kind": "table.health_critical",
                "filter": {"table_id": foreign},
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let own = table(&pool, &tenant).await;
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/rules",
            Some(json!({
                "name": "own table",
                "destination_id": destination,
                "event_kind": "table.health_critical",
                "filter": {"table_id": own},
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["filter"]["table_id"], own.to_string());

    let catalog = connection(&pool, &tenant, "lakekeeper").await;
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/rules",
            Some(json!({
                "name": "own connection",
                "destination_id": destination,
                "event_kind": "publication.commit_outcome_unknown",
                "filter": {"connection_id": catalog},
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["filter"]["connection_id"], catalog.to_string());
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_duplicate_rule_name_is_a_conflict(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let destination = destination(&pool, &tenant, "alerts").await;
    let browser =
        Browser::with_notifications(&pool, &tenant, tenant.user, "alice", delivery()).await;

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/rules",
            Some(rule_body("replans", destination)),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/rules",
            Some(rule_body("replans", destination)),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "resource_already_exists");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn only_notification_administrators_may_manage_rules(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let bob = support::user(&mut conn, tenant.organization, "bob").await;
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: bob,
            role: Role::Viewer,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    drop(conn);

    let browser = Browser::new(&pool, &tenant, bob, "bob").await;
    let (status, body) = browser
        .request("GET", "/api/v1/notifications/rules", None, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["reason"], "permission_denied");
}
