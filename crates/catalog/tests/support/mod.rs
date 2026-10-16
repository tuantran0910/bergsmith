//! A harness for the Compose Lakekeeper (`BERGSMITH_TEST_CATALOG_URL`).
//!
//! Every test gets its own warehouse, backed by RustFS with STS enabled, so
//! tests never share tables.
#![allow(dead_code, reason = "each test binary uses a different subset")]
#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "fixtures fail the test on setup errors"
)]

pub(crate) mod commit;
pub(crate) mod proxy;
pub(crate) mod sink;

use bergsmith_catalog::{CatalogConnection, ConnectionConfig, TableName};
use serde_json::json;
use url::Url;
use uuid::Uuid;

/// The Lakekeeper server root.
pub(crate) fn server() -> Url {
    let url = std::env::var("BERGSMITH_TEST_CATALOG_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:58181".to_owned());
    Url::parse(&url).expect("BERGSMITH_TEST_CATALOG_URL is a URL")
}

/// The RustFS endpoint as Lakekeeper sees it, inside the Compose network.
fn internal_s3_endpoint() -> String {
    std::env::var("BERGSMITH_TEST_S3_INTERNAL_ENDPOINT")
        .unwrap_or_else(|_| "http://rustfs:9000".to_owned())
}

/// The RustFS endpoint as the tests see it.
pub(crate) fn s3_endpoint() -> String {
    std::env::var("BERGSMITH_TEST_S3_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:59000".to_owned())
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

async fn post(url: Url, body: serde_json::Value) -> reqwest::Response {
    http()
        .post(url)
        .json(&body)
        .send()
        .await
        .expect("Lakekeeper is reachable; run `make services-up`")
}

/// A fresh warehouse in Lakekeeper.
pub(crate) struct Warehouse {
    pub(crate) name: String,
}

impl Warehouse {
    /// Bootstraps Lakekeeper if needed and creates a uniquely named warehouse
    /// whose storage profile vends STS credentials from RustFS.
    pub(crate) async fn create() -> Self {
        let bootstrap = post(
            server().join("management/v1/bootstrap").expect("valid URL"),
            json!({ "accept-terms-of-use": true }),
        )
        .await;
        assert!(
            bootstrap.status().is_success() || bootstrap.status().is_client_error(),
            "bootstrap failed: {}",
            bootstrap.status()
        );

        let name = format!("w{}", Uuid::now_v7().simple());
        let response = post(
            server().join("management/v1/warehouse").expect("valid URL"),
            json!({
                "warehouse-name": name,
                "storage-profile": {
                    "type": "s3",
                    "bucket": "warehouse",
                    "key-prefix": name,
                    "endpoint": internal_s3_endpoint(),
                    "region": "us-east-1",
                    "path-style-access": true,
                    "flavor": "s3-compat",
                    "sts-enabled": true
                },
                "storage-credential": {
                    "type": "s3",
                    "credential-type": "access-key",
                    "aws-access-key-id": "rustfsadmin",
                    "aws-secret-access-key": "rustfsadmin"
                }
            }),
        )
        .await;
        assert!(
            response.status().is_success(),
            "warehouse creation failed: {}",
            response.text().await.unwrap_or_default()
        );
        Self { name }
    }

    /// The connection settings for this warehouse.
    pub(crate) fn config(&self) -> ConnectionConfig {
        let mut config = ConnectionConfig::new(server().join("catalog").expect("valid URL"));
        config.warehouse = Some(self.name.clone());
        config
    }

    /// Connects to this warehouse.
    pub(crate) async fn connect(&self) -> CatalogConnection {
        CatalogConnection::connect(&self.config())
            .await
            .expect("connects to the warehouse")
    }

    /// Connects to this warehouse through `root` instead of Lakekeeper.
    pub(crate) async fn connect_via(&self, root: &Url) -> CatalogConnection {
        let mut config = self.config();
        config.uri = root.join("catalog").expect("valid URL");
        CatalogConnection::connect(&config)
            .await
            .expect("connects to the warehouse")
    }

    /// The warehouse's REST prefix, from `GET /v1/config`.
    pub(crate) async fn prefix(&self) -> String {
        let mut url = server().join("catalog/v1/config").expect("valid URL");
        url.query_pairs_mut().append_pair("warehouse", &self.name);
        let config: serde_json::Value = http()
            .get(url)
            .send()
            .await
            .expect("config request")
            .json()
            .await
            .expect("config body");
        config["defaults"]["prefix"]
            .as_str()
            .or_else(|| config["overrides"]["prefix"].as_str())
            .expect("Lakekeeper returns a prefix")
            .to_owned()
    }

    /// Creates `namespace.name` with a single `id long` column.
    pub(crate) async fn create_table(&self, namespace: &str, name: &str) -> TableName {
        let base = server()
            .join(&format!("catalog/v1/{}/", self.prefix().await))
            .expect("valid URL");
        let namespaces = post(
            base.join("namespaces").expect("valid URL"),
            json!({ "namespace": [namespace] }),
        )
        .await;
        assert!(
            namespaces.status().is_success() || namespaces.status().as_u16() == 409,
            "namespace creation failed: {}",
            namespaces.status()
        );
        let table = post(
            base.join(&format!("namespaces/{namespace}/tables"))
                .expect("valid URL"),
            json!({
                "name": name,
                "schema": {
                    "type": "struct",
                    "schema-id": 0,
                    "fields": [{ "id": 1, "name": "id", "required": true, "type": "long" }]
                }
            }),
        )
        .await;
        assert!(
            table.status().is_success(),
            "table creation failed: {}",
            table.text().await.unwrap_or_default()
        );
        TableName {
            namespace: vec![namespace.to_owned()],
            name: name.to_owned(),
        }
    }
}
