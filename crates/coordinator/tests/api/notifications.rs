//! Notification destination administration (BS-029.T1).

use super::*;
use bergsmith_common::notifications::AllowList;
use bergsmith_coordinator::notifications::{DeliveryClient, Settings};
use bergsmith_persistence::MIGRATOR;
use bergsmith_persistence::grants::{self, NewRoleGrant};

fn delivery(hosts: &[&str]) -> DeliveryClient {
    let settings = Settings::new(AllowList::new(hosts).unwrap(), None, Duration::from_secs(5));
    DeliveryClient::new(&settings).unwrap()
}

async fn create_webhook(browser: &Browser, name: &str) -> Uuid {
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({
                "name": name,
                "kind": "webhook",
                "url": "https://hooks.example.com/events",
                "signing_secret_ref": "ops-key",
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_destination_is_created_listed_and_edited(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::with_notifications(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        delivery(&["hooks.example.com"]),
    )
    .await;

    let id = create_webhook(&browser, "ops webhook").await;

    let (status, body) = browser
        .request("GET", "/api/v1/notifications/destinations", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], id.to_string());
    assert_eq!(body["items"][0]["kind"], "webhook");
    assert_eq!(body["items"][0]["url"], "https://hooks.example.com/events");
    assert_eq!(body["items"][0]["signing_secret_ref"], "ops-key");
    assert_eq!(body["items"][0]["enabled"], true);
    assert_eq!(body["items"][0]["revision"], 1);

    let (status, body) = browser
        .request(
            "PATCH",
            &format!("/api/v1/notifications/destinations/{id}"),
            Some(json!({"revision": 1, "name": "renamed", "enabled": false})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "renamed");
    assert_eq!(body["enabled"], false);
    assert_eq!(body["revision"], 2);
    // The kind and its secret reference are untouched by an unrelated edit.
    assert_eq!(body["signing_secret_ref"], "ops-key");

    // The revision that was already consumed no longer applies.
    let (status, body) = browser
        .request(
            "PATCH",
            &format!("/api/v1/notifications/destinations/{id}"),
            Some(json!({"revision": 1, "enabled": true})),
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
            .any(|action| action == "notification.destination_created")
    );
    assert!(
        actions
            .iter()
            .any(|action| action == "notification.destination_updated")
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn the_allow_list_and_the_scheme_are_enforced(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::with_notifications(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        delivery(&["hooks.example.com"]),
    )
    .await;

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({
                "name": "elsewhere",
                "kind": "slack",
                "url": "https://evil.example.org/hook",
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["reason"], "destination_not_allowed");

    for url in [
        "http://hooks.example.com/events",
        "https://user:pass@hooks.example.com/events",
    ] {
        let (status, body) = browser
            .request(
                "POST",
                "/api/v1/notifications/destinations",
                Some(json!({"name": "unsafe", "kind": "slack", "url": url})),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{url}: {body}");
        assert_eq!(body["reason"], "invalid_request");
    }

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({
                "name": "unknown",
                "kind": "email",
                "url": "https://hooks.example.com/events",
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn signing_secret_rules_depend_on_the_kind(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::with_notifications(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        delivery(&["hooks.example.com"]),
    )
    .await;

    // A webhook is always signed, so it needs a reference.
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({
                "name": "unsigned",
                "kind": "webhook",
                "url": "https://hooks.example.com/events",
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // A reference may not escape the secret directory.
    let (status, _) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({
                "name": "escape",
                "kind": "webhook",
                "url": "https://hooks.example.com/events",
                "signing_secret_ref": "../key",
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Slack carries no Bergsmith signature.
    let (status, _) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({
                "name": "slack signed",
                "kind": "slack",
                "url": "https://hooks.example.com/events",
                "signing_secret_ref": "ops-key",
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({
                "name": "slack",
                "kind": "slack",
                "url": "https://hooks.example.com/events",
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["signing_secret_ref"], Value::Null);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn only_notification_administrators_may_manage_destinations(pool: PgPool) {
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
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({
                "name": "ops",
                "kind": "slack",
                "url": "https://hooks.example.com/events",
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["reason"], "permission_denied");
    assert_eq!(
        browser
            .request("GET", "/api/v1/notifications/destinations", None, None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_test_delivery_reports_an_unreachable_destination(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::with_notifications(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        delivery(&["127.0.0.1:1"]),
    )
    .await;

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({
                "name": "unreachable",
                "kind": "slack",
                "url": "https://127.0.0.1:1/hook",
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap();

    let (status, body) = browser
        .request(
            "POST",
            &format!("/api/v1/notifications/destinations/{id}/test"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(body["reason"], "notification_delivery_failed");
    assert_eq!(body["disposition"], "PermanentFailure");
}
