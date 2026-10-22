//! The one-origin browser surface mounts auth, API, and the built UI (BS-020.T1).

// Fixtures fail the test immediately; helper functions are not `#[test]` items,
// so clippy's allow-*-in-tests settings do not cover them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bergsmith_coordinator::api::{ApiState, EventStreamLimits, JobEventHub};
use bergsmith_coordinator::identity::oidc::{OidcConfig, OidcProvider, TokenEndpointAuth};
use bergsmith_coordinator::identity::routes::IdentityState;
use bergsmith_coordinator::identity::session::SessionConfig;
use bergsmith_coordinator::preview::{CatalogPreviewer, PreviewProfiles};
use bergsmith_coordinator::runtime::http;
use bergsmith_maintenance::PlannerConfig;
use bergsmith_persistence::{Database, DatabaseConfig, DatabaseUrl};
use sqlx::{ConnectOptions, PgPool};
use tower::ServiceExt as _;

fn server_url(pool: &PgPool) -> String {
    pool.connect_options().to_url_lossy().to_string()
}

async fn provider(server: &mut mockito::ServerGuard) -> OidcProvider {
    let issuer = server.url();
    server
        .mock("GET", "/.well-known/openid-configuration")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "jwks_uri": format!("{issuer}/jwks"),
                "id_token_signing_alg_values_supported": ["RS256"]
            })
            .to_string(),
        )
        .create_async()
        .await;
    OidcProvider::discover(OidcConfig {
        issuer,
        client_id: "bergsmith".into(),
        client_secret: None,
        redirect_uri: url::Url::parse("http://127.0.0.1:8080/auth/callback").unwrap(),
        scopes: vec!["openid".into()],
        token_endpoint_auth: TokenEndpointAuth::None,
        allow_insecure_http: true,
        leeway: std::time::Duration::from_secs(30),
        request_timeout: std::time::Duration::from_secs(10),
        http: reqwest::Client::new(),
    })
    .await
    .unwrap()
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_ui_api_and_auth_share_one_origin(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let provider = provider(&mut server).await;
    let mut settings = DatabaseConfig::new(DatabaseUrl::new(server_url(&pool)));
    settings.max_connections = 2;
    let database = Database::connect(&settings).await.unwrap();
    let sessions = SessionConfig {
        secure: false,
        ..SessionConfig::default()
    };
    let previews =
        CatalogPreviewer::new(PreviewProfiles::default(), PlannerConfig::default()).unwrap();
    let events = JobEventHub::new(EventStreamLimits::default()).unwrap();
    let api = ApiState {
        database: database.clone(),
        sessions: sessions.clone(),
        previews,
        events,
    };
    let identity = IdentityState::new(database, Arc::new(provider), sessions);

    let web = tempfile::tempdir().unwrap();
    std::fs::write(
        web.path().join("index.html"),
        "<!doctype html><title>app</title>",
    )
    .unwrap();
    std::fs::create_dir(web.path().join("assets")).unwrap();
    std::fs::write(web.path().join("assets/app.js"), "console.log('app')").unwrap();
    let app = http::app(api, identity, Some(web.path().to_path_buf())).unwrap();

    let health = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    let index = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(index.status(), StatusCode::OK);
    assert!(
        index
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .is_some()
    );

    let contract = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(contract.status(), StatusCode::OK);
    assert_eq!(
        contract.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );

    let missing_api = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/does-not-exist")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_api.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(missing_api.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("<title>app</title>"));
}
