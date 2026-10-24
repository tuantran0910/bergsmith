//! Catalog authentication against a mock catalog and token endpoint
//! (BS-005.T1). The Compose Lakekeeper runs without authentication; these
//! supporting tests cover the OAuth2 client-credentials grant until the
//! OIDC development provider arrives (BS-017).

// clippy's `allow-unwrap-in-tests` covers only `#[test]` functions; the
// helpers below build fixed test URLs.
#![expect(clippy::unwrap_used, reason = "test helpers parse fixed URLs")]

use bergsmith_catalog::{Auth, CatalogConnection, CatalogError, ConnectionConfig};
use bergsmith_common::secret::Secret;
use mockito::{Matcher, Server};
use url::Url;

const CONFIG_BODY: &str = r#"{"defaults":{"prefix":"p1"},"overrides":{},
    "endpoints":["GET /v1/{prefix}/namespaces/{namespace}/tables",
                 "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}",
                 "POST /v1/{prefix}/namespaces/{namespace}/tables/{table}"]}"#;

fn config(server: &Server, auth: Auth) -> ConnectionConfig {
    let mut config =
        ConnectionConfig::new(Url::parse(&format!("{}/catalog", server.url())).unwrap());
    config.warehouse = Some("demo".to_owned());
    config.auth = auth;
    config
}

fn oauth2(server: &Server, secret: &str) -> Auth {
    Auth::OAuth2 {
        token_url: Url::parse(&format!("{}/oauth/token", server.url())).unwrap(),
        client_id: "bergsmith".to_owned(),
        client_secret: Secret::new(secret),
        scope: Some("catalog".to_owned()),
    }
}

#[tokio::test]
async fn oauth2_tokens_are_fetched_once_and_sent_as_bearer_tokens() {
    let mut server = Server::new_async().await;
    let token = server
        .mock("POST", "/oauth/token")
        .match_body(Matcher::AllOf(vec![
            Matcher::UrlEncoded("grant_type".into(), "client_credentials".into()),
            Matcher::UrlEncoded("client_id".into(), "bergsmith".into()),
            Matcher::UrlEncoded("client_secret".into(), "s3cret".into()),
            Matcher::UrlEncoded("scope".into(), "catalog".into()),
        ]))
        .with_body(r#"{"access_token":"t1","token_type":"bearer","expires_in":3600}"#)
        .expect(1)
        .create_async()
        .await;
    let catalog_config = server
        .mock("GET", "/catalog/v1/config")
        .match_query(Matcher::UrlEncoded("warehouse".into(), "demo".into()))
        .match_header("authorization", "Bearer t1")
        .with_body(CONFIG_BODY)
        .create_async()
        .await;
    let tables = server
        .mock(
            "GET",
            Matcher::Regex(r"^/catalog/v1/p1/namespaces/db/tables(\?.*)?$".to_owned()),
        )
        .match_header("authorization", "Bearer t1")
        .with_body(r#"{"identifiers":[{"namespace":["db"],"name":"events"}]}"#)
        .create_async()
        .await;

    let connection = CatalogConnection::connect(&config(&server, oauth2(&server, "s3cret")))
        .await
        .unwrap();
    let listed = connection
        .list_tables(&["db".to_owned()], None, 50)
        .await
        .unwrap();

    assert_eq!(listed.items.len(), 1);
    token.assert_async().await;
    catalog_config.assert_async().await;
    tables.assert_async().await;
}

#[tokio::test]
async fn a_rejected_oauth2_token_is_discarded_and_fetched_again() {
    let mut server = Server::new_async().await;
    let token = server
        .mock("POST", "/oauth/token")
        .with_body(r#"{"access_token":"t1","token_type":"bearer","expires_in":3600}"#)
        .expect(2)
        .create_async()
        .await;
    server
        .mock("GET", "/catalog/v1/config")
        .match_query(Matcher::Any)
        .with_body(CONFIG_BODY)
        .create_async()
        .await;
    let tables = Matcher::Regex(r"^/catalog/v1/p1/namespaces/db/tables(\?.*)?$".to_owned());
    let rejected = server
        .mock("GET", tables.clone())
        .with_status(401)
        .expect(1)
        .create_async()
        .await;
    let accepted = server
        .mock("GET", tables)
        .with_body(r#"{"identifiers":[]}"#)
        .expect(1)
        .create_async()
        .await;
    let connection = CatalogConnection::connect(&config(&server, oauth2(&server, "s3cret")))
        .await
        .unwrap();

    let error = connection
        .list_tables(&["db".to_owned()], None, 50)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, CatalogError::Status { status: 401, .. }),
        "{error}"
    );
    // The cached token is still within its lifetime, but the catalog
    // rejected it; the next request must not repeat it.
    assert!(
        connection
            .list_tables(&["db".to_owned()], None, 50)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    token.assert_async().await;
    rejected.assert_async().await;
    accepted.assert_async().await;
}

#[tokio::test]
async fn rejected_client_credentials_fail_to_connect() {
    let mut server = Server::new_async().await;
    server
        .mock("POST", "/oauth/token")
        .with_status(401)
        .with_body(r#"{"error":"invalid_client"}"#)
        .create_async()
        .await;

    let error = CatalogConnection::connect(&config(&server, oauth2(&server, "wrong")))
        .await
        .err()
        .unwrap();

    assert!(
        matches!(error, CatalogError::Authentication { status: 401 }),
        "{error}"
    );
    assert!(!error.to_string().contains("wrong"));
}

#[tokio::test]
async fn static_bearer_tokens_are_sent_as_is() {
    let mut server = Server::new_async().await;
    let catalog_config = server
        .mock("GET", "/catalog/v1/config")
        .match_query(Matcher::Any)
        .match_header("authorization", "Bearer static")
        .with_body(CONFIG_BODY)
        .create_async()
        .await;

    CatalogConnection::connect(&config(&server, Auth::BearerToken(Secret::new("static"))))
        .await
        .unwrap();

    catalog_config.assert_async().await;
}

#[tokio::test]
async fn catalogs_missing_required_endpoints_are_rejected() {
    let mut server = Server::new_async().await;
    server
        .mock("GET", "/catalog/v1/config")
        .match_query(Matcher::Any)
        .with_body(r#"{"defaults":{},"overrides":{},"endpoints":["GET /v1/config"]}"#)
        .create_async()
        .await;

    let error = CatalogConnection::connect(&config(&server, Auth::None))
        .await
        .err()
        .unwrap();

    let CatalogError::MissingEndpoints(missing) = error else {
        panic!("expected missing endpoints, got {error}");
    };
    assert_eq!(missing.len(), 3);
}

#[tokio::test]
async fn catalog_errors_carry_the_decoded_error_model() {
    let mut server = Server::new_async().await;
    server
        .mock("GET", "/catalog/v1/config")
        .match_query(Matcher::Any)
        .with_status(404)
        .with_body(r#"{"error":{"message":"Warehouse demo not found","type":"NotFoundException","code":404}}"#)
        .create_async()
        .await;

    let error = CatalogConnection::connect(&config(&server, Auth::None))
        .await
        .err()
        .unwrap();

    assert!(error.is_not_found());
    assert!(
        error
            .to_string()
            .contains("NotFoundException: Warehouse demo not found"),
        "{error}"
    );
}

#[tokio::test]
async fn undecodable_responses_are_reported() {
    let mut server = Server::new_async().await;
    server
        .mock("GET", "/catalog/v1/config")
        .match_query(Matcher::Any)
        .with_body("not json")
        .create_async()
        .await;

    let error = CatalogConnection::connect(&config(&server, Auth::None))
        .await
        .err()
        .unwrap();

    assert!(matches!(error, CatalogError::Decode { .. }), "{error}");
}

#[tokio::test]
async fn unreachable_catalogs_fail_in_transport() {
    let server = Url::parse("http://127.0.0.1:9/catalog").unwrap();
    let mut config = ConnectionConfig::new(server);
    config.connect_timeout = std::time::Duration::from_millis(500);

    let error = CatalogConnection::connect(&config).await.err().unwrap();

    assert!(matches!(error, CatalogError::Transport { .. }), "{error}");
}
