//! Sending commits to a mock catalog (BS-006.T3): exact bodies, idempotency
//! keys, no retries or redirects, and every transport outcome.

// clippy's `allow-*-in-tests` settings cover only `#[test]` functions; the
// helpers below build fixed URLs and fail the test on setup errors.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers fail the test on setup errors"
)]

use std::net::SocketAddr;
use std::time::Duration;

use bergsmith_catalog::{
    CatalogConnection, CatalogError, CommitOutcome, CommitRequest, ConnectionConfig,
    IdempotencyKey, RejectionKind, TableName, UnknownOutcome,
};
use mockito::{Matcher, Server};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use url::Url;

const ENDPOINTS: &str = r#""endpoints":["GET /v1/{prefix}/namespaces/{namespace}/tables",
    "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}",
    "POST /v1/{prefix}/namespaces/{namespace}/tables/{table}"]"#;
const COMMIT_PATH: &str = "/catalog/v1/p1/namespaces/db/tables/t";
const BODY: &str = r#"{"requirements":[],"updates":[]}"#;

fn config_body(idempotency: bool) -> String {
    let lifetime = if idempotency {
        r#","idempotency-key-lifetime":"PT30M""#
    } else {
        ""
    };
    format!(r#"{{"defaults":{{"prefix":"p1"}},"overrides":{{}},{ENDPOINTS}{lifetime}}}"#)
}

fn request() -> CommitRequest {
    CommitRequest::from_persisted(
        TableName {
            namespace: vec!["db".to_owned()],
            name: "t".to_owned(),
        },
        BODY.as_bytes().to_vec(),
    )
}

async fn connect(server: &mut Server, idempotency: bool) -> CatalogConnection {
    let _config = server
        .mock("GET", "/catalog/v1/config")
        .with_body(config_body(idempotency))
        .create_async()
        .await;
    let config = ConnectionConfig::new(Url::parse(&format!("{}/catalog", server.url())).unwrap());
    CatalogConnection::connect(&config).await.unwrap()
}

#[tokio::test]
async fn the_exact_body_is_sent_once_without_a_key() {
    let mut server = Server::new_async().await;
    let connection = connect(&mut server, true).await;
    let commit = server
        .mock("POST", COMMIT_PATH)
        .match_header("content-type", "application/json")
        .match_header("idempotency-key", Matcher::Missing)
        .match_body(BODY)
        .with_body("{}")
        .expect(1)
        .create_async()
        .await;

    let outcome = connection.commit(&request(), None).await.unwrap();

    assert!(
        matches!(&outcome, CommitOutcome::Committed(c) if c.metadata.is_none()),
        "{outcome:?}"
    );
    commit.assert_async().await;
}

#[tokio::test]
async fn idempotency_keys_are_sent_when_the_catalog_supports_them() {
    let mut server = Server::new_async().await;
    let connection = connect(&mut server, true).await;
    let key = IdempotencyKey::generate();
    let commit = server
        .mock("POST", COMMIT_PATH)
        .match_header("idempotency-key", key.to_string().as_str())
        .with_body("{}")
        .expect(1)
        .create_async()
        .await;

    let outcome = connection.commit(&request(), Some(key)).await.unwrap();

    assert!(
        matches!(outcome, CommitOutcome::Committed(_)),
        "{outcome:?}"
    );
    commit.assert_async().await;
}

#[tokio::test]
async fn idempotency_keys_are_refused_without_catalog_support() {
    let mut server = Server::new_async().await;
    let connection = connect(&mut server, false).await;
    let commit = server
        .mock("POST", COMMIT_PATH)
        .expect(0)
        .create_async()
        .await;

    let error = connection
        .commit(&request(), Some(IdempotencyKey::generate()))
        .await
        .unwrap_err();

    assert!(
        matches!(error, CatalogError::IdempotencyUnsupported),
        "{error}"
    );
    commit.assert_async().await;
}

#[tokio::test]
async fn conflicts_are_definitive_rejections() {
    let mut server = Server::new_async().await;
    let connection = connect(&mut server, false).await;
    let _commit = server
        .mock("POST", COMMIT_PATH)
        .with_status(409)
        .with_body(r#"{"error":{"message":"stale","type":"CommitFailedException","code":409}}"#)
        .create_async()
        .await;

    let outcome = connection.commit(&request(), None).await.unwrap();

    let CommitOutcome::Rejected(rejection) = outcome else {
        panic!("409 is a rejection: {outcome:?}");
    };
    assert_eq!(rejection.kind, RejectionKind::RequirementFailed);
    assert_eq!(rejection.model.unwrap().kind, "CommitFailedException");
}

#[tokio::test]
async fn server_errors_are_unknown_and_never_retried() {
    let mut server = Server::new_async().await;
    let connection = connect(&mut server, false).await;
    let commit = server
        .mock("POST", COMMIT_PATH)
        .with_status(503)
        .expect(1)
        .create_async()
        .await;

    let outcome = connection.commit(&request(), None).await.unwrap();

    assert!(
        matches!(
            outcome,
            CommitOutcome::Unknown(UnknownOutcome::Status { status: 503, .. })
        ),
        "{outcome:?}"
    );
    commit.assert_async().await;
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let mut server = Server::new_async().await;
    let connection = connect(&mut server, false).await;
    let _commit = server
        .mock("POST", COMMIT_PATH)
        .with_status(307)
        .with_header("location", "/elsewhere")
        .create_async()
        .await;
    let elsewhere = server
        .mock("POST", "/elsewhere")
        .expect(0)
        .create_async()
        .await;

    let outcome = connection.commit(&request(), None).await.unwrap();

    assert!(
        matches!(
            outcome,
            CommitOutcome::Unknown(UnknownOutcome::Status { status: 307, .. })
        ),
        "{outcome:?}"
    );
    elsewhere.assert_async().await;
}

/// How the raw server treats a commit.
#[derive(Clone, Copy)]
enum OnCommit {
    /// Read the request and never answer.
    Hang,
    /// Read the request and close the connection.
    Close,
}

/// A minimal HTTP server that answers `GET /v1/config` and treats commits
/// as told. Every response closes its connection, so nothing is pooled.
struct RawServer {
    addr: SocketAddr,
    tasks: JoinSet<()>,
}

impl RawServer {
    async fn start(on_commit: OnCommit) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                connections.spawn(serve(stream, on_commit));
            }
        });
        Self { addr, tasks }
    }

    async fn connect(&self) -> CatalogConnection {
        let mut config =
            ConnectionConfig::new(Url::parse(&format!("http://{}/catalog", self.addr)).unwrap());
        config.request_timeout = Duration::from_millis(300);
        CatalogConnection::connect(&config).await.unwrap()
    }

    /// Stops listening; later connections are refused.
    async fn stop(mut self) {
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
    }
}

async fn serve(mut stream: TcpStream, on_commit: OnCommit) {
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).await.unwrap();
        if read == 0 {
            return;
        }
        request.extend_from_slice(&buffer[..read]);
    }
    if request.starts_with(b"GET ") {
        let body = config_body(false);
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        return;
    }
    match on_commit {
        OnCommit::Hang => std::future::pending().await,
        OnCommit::Close => drop(stream),
    }
}

#[tokio::test]
async fn timeouts_are_unknown() {
    let server = RawServer::start(OnCommit::Hang).await;
    let connection = server.connect().await;

    let outcome = connection.commit(&request(), None).await.unwrap();

    assert!(
        matches!(outcome, CommitOutcome::Unknown(UnknownOutcome::Timeout)),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn connections_lost_after_sending_are_unknown() {
    let server = RawServer::start(OnCommit::Close).await;
    let connection = server.connect().await;

    let outcome = connection.commit(&request(), None).await.unwrap();

    assert!(
        matches!(
            outcome,
            CommitOutcome::Unknown(UnknownOutcome::Transport(_))
        ),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn refused_connections_mean_nothing_was_sent() {
    let server = RawServer::start(OnCommit::Close).await;
    let connection = server.connect().await;
    server.stop().await;

    let error = connection.commit(&request(), None).await.unwrap_err();

    assert!(matches!(error, CatalogError::Transport { .. }), "{error}");
}
