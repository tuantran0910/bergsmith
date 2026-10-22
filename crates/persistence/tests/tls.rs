//! PostgreSQL TLS tests (BS-020.T6) against the Compose server.
//!
//! `make services-up` generates a development CA and starts PostgreSQL with a
//! certificate it signed. `BERGSMITH_TEST_DATABASE_CA` names that CA; the
//! default is the path the Makefile uses.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "fixtures fail the test on setup errors"
)]

use std::path::PathBuf;
use std::time::Duration;

use bergsmith_persistence::{Database, DatabaseConfig, DatabaseTlsMode, DatabaseUrl};

fn ca_path() -> PathBuf {
    std::env::var_os("BERGSMITH_TEST_DATABASE_CA").map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../deploy/docker/.pki/postgres/ca.crt")
        },
        PathBuf::from,
    )
}

fn unrelated_ca_path() -> PathBuf {
    ca_path().with_file_name("other-ca.crt")
}

fn url_with(parameters: &str) -> DatabaseUrl {
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL names the test server");
    let separator = if base.contains('?') { '&' } else { '?' };
    DatabaseUrl::new(format!("{base}{separator}{parameters}"))
}

async fn connect(url: DatabaseUrl) -> Result<Database, bergsmith_persistence::PersistenceError> {
    let mut config = DatabaseConfig::new(url);
    config.max_connections = 1;
    config.acquire_timeout = Duration::from_secs(10);
    Database::connect(&config).await
}

async fn session_is_encrypted(database: &Database) -> bool {
    sqlx::query_scalar("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")
        .fetch_one(database.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn require_negotiates_an_encrypted_session() {
    let url = url_with("sslmode=require");
    assert_eq!(url.tls_mode().unwrap(), DatabaseTlsMode::Require);

    let database = connect(url).await.unwrap();

    assert!(session_is_encrypted(&database).await);
}

#[tokio::test]
async fn verify_full_connects_with_the_trusted_ca() {
    let url = url_with(&format!(
        "sslmode=verify-full&sslrootcert={}",
        ca_path().display()
    ));
    assert!(url.tls_mode().unwrap().is_verified());

    let database = connect(url).await.unwrap();

    assert!(session_is_encrypted(&database).await);
}

#[tokio::test]
async fn verify_full_rejects_a_server_signed_by_an_untrusted_ca() {
    let url = url_with(&format!(
        "sslmode=verify-full&sslrootcert={}",
        unrelated_ca_path().display()
    ));

    let error = connect(url).await.unwrap_err();

    // rustls reports the untrusted chain through the handshake's I/O error.
    let debug = format!("{error:?}");
    assert!(
        matches!(error, bergsmith_persistence::PersistenceError::Database(_))
            && debug.contains("UnknownIssuer"),
        "{debug}"
    );
}

#[tokio::test]
async fn disable_keeps_the_development_plaintext_session() {
    let database = connect(url_with("sslmode=disable")).await.unwrap();

    assert!(!session_is_encrypted(&database).await);
}
