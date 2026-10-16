//! Smoke tests for the pinned iceberg-rust REST catalog client (BS-002).
//!
//! They confirm the client builds on the pinned toolchain and validates its
//! configuration before any network call. Catalog behavior is tested against
//! Lakekeeper in BS-005.

use std::collections::HashMap;

use iceberg::{CatalogBuilder, ErrorKind};
use iceberg_catalog_rest::{REST_CATALOG_PROP_URI, RestCatalogBuilder};

#[tokio::test]
async fn rest_catalog_requires_a_uri() {
    let error = RestCatalogBuilder::default()
        .load("bergsmith", HashMap::new())
        .await
        .expect_err("a catalog without a uri is rejected");

    assert_eq!(error.kind(), ErrorKind::DataInvalid);
}

#[tokio::test]
async fn rest_catalog_loads_without_contacting_the_server() {
    let props = HashMap::from([(
        REST_CATALOG_PROP_URI.to_owned(),
        "http://127.0.0.1:9/catalog".to_owned(),
    )]);

    RestCatalogBuilder::default()
        .load("bergsmith", props)
        .await
        .expect("loading only validates configuration");
}
