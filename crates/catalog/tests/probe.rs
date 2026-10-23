//! Reading what a catalog advertises without requiring it (BS-022.T2).
//!
//! A capability check has to say what a catalog lacks, so the probe never
//! refuses a catalog for a missing endpoint. It reads `GET /v1/config` and
//! nothing else.

// clippy's `allow-*-in-tests` settings cover only `#[test]` functions; the
// helpers below build fixed URLs and fail the test on setup errors.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers fail the test on setup errors"
)]

use bergsmith_catalog::{CatalogConnection, CatalogError, ConnectionConfig};
use mockito::Server;
use url::Url;

const ENDPOINTS: &str = r#"["GET /v1/{prefix}/namespaces/{namespace}/tables",
    "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}",
    "POST /v1/{prefix}/namespaces/{namespace}/tables/{table}",
    "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}/credentials"]"#;

fn config_body(endpoints: Option<&str>, idempotency: Option<&str>) -> String {
    let mut body = String::from(r#"{"defaults":{"prefix":"p1"},"overrides":{}"#);
    if let Some(endpoints) = endpoints {
        body.push_str(",\"endpoints\":");
        body.push_str(endpoints);
    }
    if let Some(idempotency) = idempotency {
        body.push_str(",\"idempotency-key-lifetime\":");
        body.push_str(idempotency);
    }
    body.push('}');
    body
}

async fn config(server: &mut Server, body: String) -> ConnectionConfig {
    let _config = server
        .mock("GET", "/catalog/v1/config")
        .with_body(body)
        .create_async()
        .await;
    ConnectionConfig::new(Url::parse(&format!("{}/catalog", server.url())).unwrap())
}

#[tokio::test]
async fn the_probe_reports_what_a_catalog_advertises() {
    let mut server = Server::new_async().await;
    let config = config(
        &mut server,
        config_body(Some(ENDPOINTS), Some(r#""PT30M""#)),
    )
    .await;

    let report = CatalogConnection::probe(&config).await.unwrap();

    assert!(report.supports_list_tables());
    assert!(report.supports_load_table());
    assert!(report.supports_commit_table());
    assert!(report.credentials_refresh);
    assert_eq!(
        report.idempotency_key_lifetime,
        Some(std::time::Duration::from_secs(1800))
    );
    assert!(report.missing_required().is_empty());
}

#[tokio::test]
async fn the_probe_reports_a_catalog_that_lacks_an_endpoint() {
    let mut server = Server::new_async().await;
    let config = config(
        &mut server,
        config_body(
            Some(r#"["GET /v1/{prefix}/namespaces/{namespace}/tables"]"#),
            None,
        ),
    )
    .await;

    let report = CatalogConnection::probe(&config).await.unwrap();

    assert!(report.supports_list_tables());
    assert!(!report.supports_load_table());
    assert!(!report.supports_commit_table());
    assert!(!report.credentials_refresh);
    assert_eq!(
        report.missing_required(),
        [
            "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}",
            "POST /v1/{prefix}/namespaces/{namespace}/tables/{table}"
        ]
    );
    // The same catalog is refused for work, which is why the probe exists.
    let refused = CatalogConnection::connect(&config).await;
    assert!(matches!(refused, Err(CatalogError::MissingEndpoints(_))));
}

#[tokio::test]
async fn the_probe_assumes_the_default_endpoints_when_none_are_advertised() {
    let mut server = Server::new_async().await;
    let config = config(&mut server, config_body(None, None)).await;

    let report = CatalogConnection::probe(&config).await.unwrap();

    assert!(report.supports_list_tables());
    assert!(report.supports_load_table());
    assert!(report.supports_commit_table());
    assert!(!report.credentials_refresh);
    assert!(report.idempotency_key_lifetime.is_none());
    assert!(report.missing_required().is_empty());
}

#[tokio::test]
async fn a_chunked_answer_larger_than_the_limit_is_refused() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // A chunked answer has no content length, so the limit has to hold while
    // the body is read, not before it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 1024];
        let _ = stream.read(&mut buffer).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        for _ in 0..8 {
            let chunk = vec![b'x'; 16 * 1024];
            stream
                .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                .await
                .unwrap();
            stream.write_all(&chunk).await.unwrap();
            stream.write_all(b"\r\n").await.unwrap();
        }
        stream.write_all(b"0\r\n\r\n").await.unwrap();
    });
    let config = ConnectionConfig::new(Url::parse(&format!("http://{address}/catalog")).unwrap());

    let refused = CatalogConnection::probe(&config).await;

    assert!(
        matches!(refused, Err(CatalogError::ResponseTooLarge { .. })),
        "{refused:?}"
    );
}
