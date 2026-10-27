//! Notification destination administration (BS-029.T1).

use super::*;
use bergsmith_common::notifications::AllowList;
use bergsmith_coordinator::notifications::{DeliveryClient, Settings};
use bergsmith_persistence::MIGRATOR;
use bergsmith_persistence::grants::{self, NewRoleGrant};
use sqlx::postgres::PgPoolOptions;
use std::path::{Path, PathBuf};

/// A fresh temp directory holding the named files, one per secret reference.
async fn secrets(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bergsmith-notify-{name}-{}", std::process::id()));
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.unwrap();
    for (file, content) in files {
        tokio::fs::write(dir.join(file), content).await.unwrap();
    }
    dir
}

fn delivery(hosts: &[&str], secrets: Option<&Path>) -> DeliveryClient {
    let settings = Settings::new(
        AllowList::new(hosts).unwrap(),
        secrets.map(Path::to_path_buf),
        Duration::from_secs(5),
    );
    DeliveryClient::new(&settings).unwrap()
}

async fn create_webhook(browser: &Browser, name: &str, url_ref: &str) -> Uuid {
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({
                "name": name,
                "kind": "webhook",
                "url_ref": url_ref,
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
    let dir = secrets(
        "created",
        &[("ops-url", "https://hooks.example.com/events")],
    )
    .await;
    let browser = Browser::with_notifications(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        delivery(&["hooks.example.com"], Some(&dir)),
    )
    .await;

    let id = create_webhook(&browser, "ops webhook", "ops-url").await;

    let (status, body) = browser
        .request("GET", "/api/v1/notifications/destinations", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], id.to_string());
    assert_eq!(body["items"][0]["kind"], "webhook");
    // The reference is echoed; the URL itself is never in the contract.
    assert_eq!(body["items"][0]["url_ref"], "ops-url");
    assert!(body["items"][0].get("url").is_none());
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
    let dir = secrets(
        "policy",
        &[
            ("evil", "https://evil.example.org/hook"),
            ("http", "http://hooks.example.com/events"),
            ("userinfo", "https://user:pass@hooks.example.com/events"),
            ("known", "https://hooks.example.com/events"),
        ],
    )
    .await;
    let browser = Browser::with_notifications(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        delivery(&["hooks.example.com"], Some(&dir)),
    )
    .await;

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({"name": "elsewhere", "kind": "slack", "url_ref": "evil"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["reason"], "destination_not_allowed");

    for url_ref in ["http", "userinfo"] {
        let (status, body) = browser
            .request(
                "POST",
                "/api/v1/notifications/destinations",
                Some(json!({"name": "unsafe", "kind": "slack", "url_ref": url_ref})),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{url_ref}: {body}");
        assert_eq!(body["reason"], "destination_url_invalid");
    }

    // A reference whose file is missing is refused, never stored.
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({"name": "missing", "kind": "slack", "url_ref": "absent"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["reason"], "destination_url_unavailable");

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({"name": "unknown", "kind": "email", "url_ref": "known"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // No URL can resolve without a configured directory.
    let unconfigured = Browser::with_notifications(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        delivery(&["hooks.example.com"], None),
    )
    .await;
    let (status, body) = unconfigured
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({"name": "no dir", "kind": "slack", "url_ref": "known"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["reason"], "notifications_secrets_dir_unconfigured");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn signing_secret_rules_depend_on_the_kind(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let dir = secrets(
        "signing",
        &[("ops-url", "https://hooks.example.com/events")],
    )
    .await;
    let browser = Browser::with_notifications(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        delivery(&["hooks.example.com"], Some(&dir)),
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
                "url_ref": "ops-url",
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
                "url_ref": "ops-url",
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
                "url_ref": "ops-url",
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
            Some(json!({"name": "slack", "kind": "slack", "url_ref": "ops-url"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["signing_secret_ref"], Value::Null);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_duplicate_destination_name_is_a_conflict(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let dir = secrets(
        "duplicate",
        &[("ops-url", "https://hooks.example.com/events")],
    )
    .await;
    let browser = Browser::with_notifications(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        delivery(&["hooks.example.com"], Some(&dir)),
    )
    .await;

    create_webhook(&browser, "ops", "ops-url").await;
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({
                "name": "ops",
                "kind": "webhook",
                "url_ref": "ops-url",
                "signing_secret_ref": "ops-key",
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "resource_already_exists");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_slack_token_never_reaches_the_ledger_the_audit_trail_or_a_response(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let dir = secrets(
        "slack",
        &[(
            "slack-url",
            "https://hooks.example.com/services/T000/B000/supersecret",
        )],
    )
    .await;
    let browser = Browser::with_notifications(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        delivery(&["hooks.example.com"], Some(&dir)),
    )
    .await;

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({"name": "alerts", "kind": "slack", "url_ref": "slack-url"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["url_ref"], "slack-url");
    assert!(body.get("url").is_none());
    assert!(!body.to_string().contains("supersecret"), "{body}");

    // Neither the row nor the audit detail holds the URL.
    let row: String = sqlx::query_scalar(
        "SELECT to_jsonb(d)::text FROM notification_destinations d
         WHERE organization_id = $1",
    )
    .bind(tenant.organization.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!row.contains("supersecret"), "{row}");
    let row: Value = serde_json::from_str(&row).unwrap();
    assert_eq!(row["url_ref"], "slack-url");

    let audit: String = sqlx::query_scalar(
        "SELECT detail::text FROM audit_events
         WHERE organization_id = $1 AND action = 'notification.destination_created'",
    )
    .bind(tenant.organization.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!audit.contains("supersecret"), "{audit}");
    assert!(audit.contains("slack-url"), "{audit}");
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
            Some(json!({"name": "ops", "kind": "slack", "url_ref": "ops-url"})),
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
async fn a_test_delivery_reports_an_unreachable_destination_and_is_audited(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let dir = secrets(
        "unreachable",
        &[("unreachable-url", "https://127.0.0.1:1/hook")],
    )
    .await;
    let browser = Browser::with_notifications(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        delivery(&["127.0.0.1:1"], Some(&dir)),
    )
    .await;

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({
                "name": "unreachable",
                "kind": "slack",
                "url_ref": "unreachable-url",
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

    // A failed attempt is audited with a stable reason, because the receiver
    // may already have accepted a request whose response was lost.
    let audits: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT action, outcome, detail FROM audit_events
         WHERE organization_id = $1 AND action = 'notification.destination_tested'",
    )
    .bind(tenant.organization.as_uuid())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(audits.len(), 1, "{audits:?}");
    assert_eq!(audits[0].0, "notification.destination_tested");
    assert_eq!(audits[0].1, "Failed");
    assert_eq!(audits[0].2["delivered"], false);
    assert_eq!(audits[0].2["reason"], "notification_delivery_failed");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn one_connection_serves_creation_and_editing(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);

    // One slot is enough only if no handler holds an authorization connection
    // while taking a transaction.
    let options = (*pool.connect_options()).clone();
    let limited = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect_with(options)
        .await
        .unwrap();
    let dir = secrets(
        "limited",
        &[("ops-url", "https://hooks.example.com/events")],
    )
    .await;
    let browser = Browser::with_notifications(
        &limited,
        &tenant,
        tenant.user,
        "alice",
        delivery(&["hooks.example.com"], Some(&dir)),
    )
    .await;

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/notifications/destinations",
            Some(json!({
                "name": "ops webhook",
                "kind": "webhook",
                "url_ref": "ops-url",
                "signing_secret_ref": "ops-key",
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap();

    let (status, body) = browser
        .request(
            "PATCH",
            &format!("/api/v1/notifications/destinations/{id}"),
            Some(json!({"revision": 1, "enabled": false})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], false);
}
