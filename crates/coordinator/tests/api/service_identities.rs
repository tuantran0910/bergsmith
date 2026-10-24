//! Service identity and worker pool token administration (BS-023.T4).

use super::*;
use bergsmith_common::ids::{PrincipalId, WorkerPoolTokenId};
use bergsmith_persistence::MIGRATOR;
use bergsmith_persistence::worker_tokens;
use bergsmith_protocol::identity::ServiceToken;

fn token_of(body: &Value) -> ServiceToken {
    ServiceToken::parse(body["token"].as_str().unwrap()).unwrap()
}

async fn create_service(browser: &Browser, name: &str) -> Uuid {
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/service-identities",
            Some(json!({ "display_name": name })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"], "Service");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

async fn issue(browser: &Browser, principal: Uuid, pool: &str) -> (Uuid, ServiceToken) {
    let (status, body) = browser
        .request(
            "POST",
            &format!("/api/v1/service-identities/{principal}/tokens"),
            Some(json!({ "pool": pool, "label": "worker-1" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    (id, token_of(&body))
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_service_identity_is_created_listed_and_read(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let principal = create_service(&browser, "native workers").await;

    let (status, body) = browser
        .request("GET", "/api/v1/service-identities", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], principal.to_string());
    assert!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["kind"] == "Service")
    );

    let (status, body) = browser
        .request(
            "GET",
            &format!("/api/v1/service-identities/{principal}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["issuer"], "bergsmith:service");
    assert_eq!(body["active"], true);

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
            .any(|action| action == "identity.provisioned")
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_bearer_token_is_shown_once_and_revocation_is_immediate(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let principal = create_service(&browser, "native workers").await;
    let (token_id, token) = issue(&browser, principal, "native").await;
    assert_eq!(token.pool(), "native");
    assert!(token.expose_secret().starts_with("bsw1_native_"));

    let mut conn = pool.acquire().await.unwrap();
    let identity = worker_tokens::authenticate(&mut conn, "native", &token.verifier())
        .await
        .unwrap()
        .expect("the issued token authenticates");
    assert_eq!(identity.principal, PrincipalId::from_uuid(principal));
    drop(conn);

    let (status, body) = browser
        .request(
            "GET",
            &format!("/api/v1/service-identities/{principal}/tokens"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert!(body["items"][0].get("token").is_none(), "{body}");
    assert!(body["items"][0].get("verifier").is_none(), "{body}");
    assert_eq!(body["items"][0]["active"], true);

    let (status, body) = browser
        .request(
            "DELETE",
            &format!("/api/v1/service-identities/{principal}/tokens/{token_id}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["active"], false);

    let mut conn = pool.acquire().await.unwrap();
    assert!(
        worker_tokens::authenticate(&mut conn, "native", &token.verifier())
            .await
            .unwrap()
            .is_none(),
        "a revoked token authenticates nothing"
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn rotation_revokes_the_predecessor_and_returns_the_replacement_once(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let principal = create_service(&browser, "native workers").await;
    let (old_id, old) = issue(&browser, principal, "native").await;

    let (status, body) = browser
        .request(
            "POST",
            &format!("/api/v1/service-identities/{principal}/tokens/{old_id}/rotate"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let new_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    assert_ne!(new_id, old_id);
    assert_eq!(body["rotated_from"], old_id.to_string());
    assert_eq!(body["label"], "worker-1");
    let new = token_of(&body);

    let mut conn = pool.acquire().await.unwrap();
    assert!(
        worker_tokens::authenticate(&mut conn, "native", &old.verifier())
            .await
            .unwrap()
            .is_none(),
        "the predecessor stops authenticating in the rotation transaction"
    );
    let identity = worker_tokens::authenticate(&mut conn, "native", &new.verifier())
        .await
        .unwrap()
        .expect("the replacement authenticates");
    assert_eq!(identity.principal, PrincipalId::from_uuid(principal));
    drop(conn);

    let (status, body) = browser
        .request(
            "POST",
            &format!("/api/v1/service-identities/{principal}/tokens/{old_id}/rotate"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, body) = browser
        .request(
            "GET",
            &format!("/api/v1/service-identities/{principal}/tokens"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tokens = body["items"].as_array().unwrap();
    assert_eq!(tokens.len(), 2);
    let old = tokens
        .iter()
        .find(|token| token["id"] == old_id.to_string())
        .unwrap();
    assert_eq!(old["active"], false);
    let rotated: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_events WHERE organization_id = $1 ORDER BY id",
    )
    .bind(tenant.organization.as_uuid())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(rotated.iter().any(|action| action == "worker_token.issued"));
    assert!(
        rotated
            .iter()
            .any(|action| action == "worker_token.rotated")
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_token_requires_an_active_service_identity_and_a_valid_pool(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let person = support::user(&mut conn, tenant.organization, "bob").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let principal = create_service(&browser, "native workers").await;

    for pool_name in ["", "Native", "-native", "with_underscore", &"a".repeat(64)] {
        let (status, body) = browser
            .request(
                "POST",
                &format!("/api/v1/service-identities/{principal}/tokens"),
                Some(json!({ "pool": pool_name })),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{pool_name}: {body}");
    }

    let (status, body) = browser
        .request(
            "POST",
            &format!("/api/v1/service-identities/{person}/tokens"),
            Some(json!({ "pool": "native" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = browser
        .request(
            "POST",
            &format!("/api/v1/service-identities/{principal}/tokens"),
            Some(json!({ "pool": "native", "label": "" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn service_identity_administration_needs_workers_manage(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let bob = support::user(&mut conn, tenant.organization, "bob").await;
    drop(conn);
    let viewer = Browser::new(&pool, &tenant, bob, "bob").await;

    let (status, body) = viewer
        .request("GET", "/api/v1/service-identities", None, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = viewer
        .request(
            "POST",
            "/api/v1/service-identities",
            Some(json!({ "display_name": "workers" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let denied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE organization_id = $1 AND action = 'api.permission_denied'",
    )
    .bind(tenant.organization.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(denied, 2);
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_token_of_another_principal_is_not_reachable(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let first = create_service(&browser, "first").await;
    let second = create_service(&browser, "second").await;
    let (token_id, _) = issue(&browser, first, "native").await;

    let (status, body) = browser
        .request(
            "GET",
            &format!("/api/v1/service-identities/{second}/tokens/{token_id}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, _) = browser
        .request(
            "DELETE",
            &format!("/api/v1/service-identities/{second}/tokens/{token_id}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let mut conn = pool.acquire().await.unwrap();
    assert!(
        worker_tokens::token(
            &mut conn,
            tenant.organization,
            PrincipalId::from_uuid(first),
            WorkerPoolTokenId::from_uuid(token_id),
        )
        .await
        .is_ok(),
        "the first principal's token stays active"
    );
}

#[sqlx::test(migrator = "MIGRATOR")]
async fn a_deactivated_service_identity_cannot_issue_or_rotate_but_can_be_revoked(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let principal = create_service(&browser, "native workers").await;
    let (token_id, _) = issue(&browser, principal, "native").await;

    let (status, body) = browser
        .request(
            "PATCH",
            &format!("/api/v1/principals/{principal}"),
            Some(json!({ "active": false })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = browser
        .request(
            "POST",
            &format!("/api/v1/service-identities/{principal}/tokens"),
            Some(json!({ "pool": "native" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = browser
        .request(
            "POST",
            &format!("/api/v1/service-identities/{principal}/tokens/{token_id}/rotate"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Revocation stays available so a retired identity's tokens can be closed.
    let (status, body) = browser
        .request(
            "DELETE",
            &format!("/api/v1/service-identities/{principal}/tokens/{token_id}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["active"], false);
}
