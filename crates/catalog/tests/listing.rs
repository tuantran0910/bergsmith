//! Paged namespace and table listings against a mock catalog (BS-022.T4).
//!
//! A browse must never enumerate an unbounded namespace, must never hand a
//! caller an identifier it could not use, and must let the caller reach every
//! entry the catalog holds.

// clippy's `allow-*-in-tests` settings cover only `#[test]` functions; the
// helpers below build fixed URLs and fail the test on setup errors.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers fail the test on setup errors"
)]

use bergsmith_catalog::{CatalogConnection, CatalogError, ConnectionConfig, MAX_LISTING_ITEMS};
use mockito::{Matcher, Server};
use url::Url;

const ENDPOINTS: &str = r#"["GET /v1/{prefix}/namespaces",
    "GET /v1/{prefix}/namespaces/{namespace}/tables",
    "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}",
    "POST /v1/{prefix}/namespaces/{namespace}/tables/{table}"]"#;

async fn connect(server: &mut Server) -> CatalogConnection {
    let _config = server
        .mock("GET", "/catalog/v1/config")
        .with_body(format!(
            r#"{{"defaults":{{"prefix":"p1"}},"overrides":{{}},"endpoints":{ENDPOINTS}}}"#
        ))
        .create_async()
        .await;
    let config = ConnectionConfig::new(Url::parse(&format!("{}/catalog", server.url())).unwrap());
    CatalogConnection::connect(&config).await.unwrap()
}

#[tokio::test]
async fn namespaces_are_listed_under_a_parent_and_sorted() {
    let mut server = Server::new_async().await;
    let listing = server
        .mock("GET", "/catalog/v1/p1/namespaces")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("parent".into(), "sales\u{1f}eu".into()),
            Matcher::UrlEncoded("pageSize".into(), "50".into()),
        ]))
        .with_body(
            r#"{"namespaces":[["sales","eu","west"],["sales","eu","east"]],"next-page-token":null}"#,
        )
        .expect(1)
        .create_async()
        .await;
    let catalog = connect(&mut server).await;

    let listed = catalog
        .list_namespaces(Some(&["sales".to_owned(), "eu".to_owned()]), None, 50)
        .await
        .unwrap();

    assert_eq!(
        listed.items,
        [
            ["sales".to_owned(), "eu".to_owned(), "east".to_owned()],
            ["sales".to_owned(), "eu".to_owned(), "west".to_owned()],
        ]
    );
    assert_eq!(listed.next_page_token, None);
    assert_eq!(listed.skipped, 0);
    listing.assert_async().await;
}

#[tokio::test]
async fn a_page_carries_the_catalogs_own_continuation_token() {
    let mut server = Server::new_async().await;
    let first = server
        .mock("GET", "/catalog/v1/p1/namespaces/db/tables")
        .match_query(Matcher::AllOf(vec![
            Matcher::Any,
            Matcher::UrlEncoded("pageSize".into(), "2".into()),
        ]))
        .with_body(
            r#"{"identifiers":[{"namespace":["db"],"name":"events"}],"next-page-token":"p2"}"#,
        )
        .expect(1)
        .create_async()
        .await;
    let second = server
        .mock("GET", "/catalog/v1/p1/namespaces/db/tables")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("pageToken".into(), "p2".into()),
            Matcher::UrlEncoded("pageSize".into(), "2".into()),
        ]))
        .with_body(
            r#"{"identifiers":[{"namespace":["db"],"name":"audit"}],"next-page-token":null}"#,
        )
        .expect(1)
        .create_async()
        .await;
    let catalog = connect(&mut server).await;

    let page = catalog
        .list_tables(&["db".to_owned()], None, 2)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.next_page_token.as_deref(), Some("p2"));

    // The token is the catalog's, so the second page continues exactly there.
    let page = catalog
        .list_tables(&["db".to_owned()], Some("p2"), 2)
        .await
        .unwrap();
    assert_eq!(
        page.items
            .iter()
            .map(|table| table.name.as_str())
            .collect::<Vec<_>>(),
        ["audit"]
    );
    assert_eq!(page.next_page_token, None);
    first.assert_async().await;
    second.assert_async().await;
}

#[tokio::test]
async fn unaddressable_identifiers_are_skipped_and_counted() {
    let mut server = Server::new_async().await;
    let long = "a".repeat(256);
    // An empty level, an oversized level, a control character, a rootless
    // namespace, and one 4 MiB level: none can be registered, so none is
    // returned, and the page still answers with what it can address.
    let body = serde_json::json!({
        "identifiers": [
            {"namespace": ["db"], "name": "events"},
            {"namespace": [""], "name": "empty-level"},
            {"namespace": ["db"], "name": long},
            {"namespace": ["db"], "name": "bad\nname"},
            {"namespace": [], "name": "rootless"},
            {"namespace": ["db"], "name": "a".repeat(4 * 1024 * 1024)},
        ],
        "next-page-token": null,
    });
    let listing = server
        .mock("GET", "/catalog/v1/p1/namespaces/db/tables")
        .match_query(Matcher::Any)
        .with_body(body.to_string())
        .expect(1)
        .create_async()
        .await;
    let catalog = connect(&mut server).await;

    let page = catalog
        .list_tables(&["db".to_owned()], None, 50)
        .await
        .unwrap();

    assert_eq!(
        page.items
            .iter()
            .map(|table| table.name.as_str())
            .collect::<Vec<_>>(),
        ["events"]
    );
    assert_eq!(page.skipped, 5);
    listing.assert_async().await;
}

#[tokio::test]
async fn a_page_over_the_item_bound_is_refused() {
    let mut server = Server::new_async().await;
    let identifiers: Vec<serde_json::Value> = (0..=MAX_LISTING_ITEMS)
        .map(|index| serde_json::json!({"namespace": ["db"], "name": format!("t{index}")}))
        .collect();
    let listing = server
        .mock("GET", "/catalog/v1/p1/namespaces/db/tables")
        .match_query(Matcher::Any)
        .with_body(
            serde_json::json!({"identifiers": identifiers, "next-page-token": null}).to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    let catalog = connect(&mut server).await;

    let error = catalog
        .list_tables(&["db".to_owned()], None, 50)
        .await
        .unwrap_err();

    assert!(
        matches!(error, CatalogError::ListingOverflow { .. }),
        "{error}"
    );
    listing.assert_async().await;
}

#[tokio::test]
async fn a_page_over_the_identifier_budget_is_refused() {
    let mut server = Server::new_async().await;
    // Each addressable name is deep and long; a few hundred exceed the page
    // budget long before the item bound.
    let level = "a".repeat(255);
    let levels: Vec<String> = std::iter::repeat_n(level, 14).collect();
    let identifiers: Vec<serde_json::Value> = (0..300)
        .map(|index| serde_json::json!({"namespace": levels, "name": format!("table-{index}")}))
        .collect();
    let listing = server
        .mock("GET", "/catalog/v1/p1/namespaces/db/tables")
        .match_query(Matcher::Any)
        .with_body(
            serde_json::json!({"identifiers": identifiers, "next-page-token": null}).to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    let catalog = connect(&mut server).await;

    let error = catalog
        .list_tables(&["db".to_owned()], None, 50)
        .await
        .unwrap_err();

    assert!(
        matches!(error, CatalogError::ListingOverflow { .. }),
        "{error}"
    );
    listing.assert_async().await;
}

#[tokio::test]
async fn a_namespace_listing_names_its_missing_parent() {
    let mut server = Server::new_async().await;
    let missing = server
        .mock("GET", "/catalog/v1/p1/namespaces")
        .match_query(Matcher::Any)
        .with_status(404)
        .expect(1)
        .create_async()
        .await;
    let catalog = connect(&mut server).await;

    let error = catalog
        .list_namespaces(Some(&["nope".to_owned()]), None, 10)
        .await
        .unwrap_err();

    assert!(error.is_not_found(), "{error}");
    missing.assert_async().await;
}
