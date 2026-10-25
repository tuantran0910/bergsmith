//! Table statistics contracts (BS-025.T3).
//!
//! The samples are inserted through the repository; these tests cover the
//! HTTP contract, the cursor, and the grant-filtered aggregates.

use super::*;

use bergsmith_common::ids::CatalogConnectionId;
use bergsmith_persistence::statistics as samples;
use bergsmith_persistence::tables;
use chrono::{TimeDelta, Utc};
use serde_json::json;
use uuid::Uuid;

/// A sample at `at` with `record_count` rows.
fn sample(
    connection: CatalogConnectionId,
    at: chrono::DateTime<Utc>,
    record_count: u64,
) -> samples::NewTableSample {
    samples::NewTableSample {
        catalog_connection_id: connection,
        sampled_at: at,
        metadata_location: format!("file:///t/metadata/{record_count}.json"),
        snapshot_id: Some(7),
        last_operation: Some("append".to_owned()),
        record_count,
        data_files: 3,
        data_bytes: 4_096,
        delete_files: 1,
        delete_bytes: 512,
        position_delete_files: 1,
        equality_delete_files: 0,
        position_delete_records: 10,
        equality_delete_records: 0,
        manifest_count: 2,
        manifest_bytes: 2_048,
        partition_count: 2,
        small_files: 1,
        target_file_size_bytes: 1_048_576,
        snapshot_count: 4,
        oldest_snapshot_at: Some(at - TimeDelta::days(1)),
        newest_snapshot_at: Some(at),
        size_histogram: json!([
            {"lower_bound_bytes": 0, "upper_bound_bytes": 1024, "files": 1, "bytes": 512},
            {"lower_bound_bytes": 1024, "upper_bound_bytes": null, "files": 2, "bytes": 3584}
        ]),
        top_partitions: json!([
            {"spec_id": 0, "partition": "day=2026-01-01", "files": 2, "bytes": 2048}
        ]),
    }
}

async fn record(
    pool: &PgPool,
    tenant: &support::Tenant,
    table: bergsmith_common::ids::TableId,
    at: chrono::DateTime<Utc>,
    record_count: u64,
) {
    let mut conn = pool.acquire().await.unwrap();
    samples::record(
        &mut conn,
        tenant.organization,
        table,
        &sample(tenant.connection, at, record_count),
    )
    .await
    .unwrap();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn table_statistics_expose_precision_safe_samples_and_history(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "sampled").await;
    let foreign = support::tenant(&mut conn, "foreign-sampled").await;
    drop(conn);
    let base = chrono::DateTime::from_timestamp_millis(1_767_225_600_000).unwrap();
    record(&pool, &tenant, tenant.table, base, 120).await;
    record(
        &pool,
        &tenant,
        tenant.table,
        base + TimeDelta::hours(1),
        200,
    )
    .await;
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let uri = format!("/api/v1/tables/{}/statistics", tenant.table);
    let (status, body) = browser.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let latest = &body["latest"];
    assert_eq!(latest["record_count"], "200");
    assert_eq!(latest["data_files"], "3");
    assert_eq!(latest["data_bytes"], "4096");
    assert_eq!(latest["small_files"], "1");
    assert_eq!(latest["snapshot_id"], "7");
    assert_eq!(latest["last_operation"], "append");
    assert_eq!(
        latest["catalog_connection_id"],
        tenant.connection.to_string()
    );
    assert_eq!(latest["size_histogram"][1]["files"], "2");
    assert_eq!(
        latest["size_histogram"][1]["upper_bound_bytes"],
        Value::Null
    );
    assert_eq!(latest["top_partitions"][0]["partition"], "day=2026-01-01");
    assert!(
        latest["newest_snapshot_age_seconds"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .is_ok()
    );
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["items"][0]["record_count"], "200");
    assert_eq!(body["items"][1]["record_count"], "120");
    assert!(body["next_cursor"].is_null());

    let since =
        (base + TimeDelta::minutes(30)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (status, body) = browser
        .request("GET", &format!("{uri}?since={since}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["latest"]["record_count"], "200");

    let (status, page) = browser
        .request("GET", &format!("{uri}?limit=1"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let cursor = page["next_cursor"].as_str().unwrap().to_owned();
    let (status, next) = browser
        .request("GET", &format!("{uri}?limit=1&cursor={cursor}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{next}");
    assert_eq!(next["items"][0]["record_count"], "120");
    assert!(next["next_cursor"].is_null());

    // A viewer sees the counts but not the partition values, which are
    // metadata only a principal allowed to plan may read.
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    let (status, body) = viewer.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["latest"]["partition_count"], "2");
    assert!(
        body["latest"]["top_partitions"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // The table list carries the newest summary so columns can sort it.
    let (status, tables) = browser.request("GET", "/api/v1/tables", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tables["items"][0]["statistics"]["record_count"], "200");

    // A table in another organization is indistinguishable from a missing one.
    let foreign_browser = Browser::new(&pool, &foreign, foreign.user, "alice").await;
    assert_eq!(
        foreign_browser.request("GET", &uri, None, None).await.0,
        StatusCode::NOT_FOUND
    );
    let (status, body) = browser
        .request(
            "GET",
            &format!("/api/v1/tables/{}/statistics", Uuid::now_v7()),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["reason"], "resource_not_found");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn aggregates_cover_only_visible_tables(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "totals").await;
    let other_connection = support::connection(&mut conn, tenant.organization, "second").await;
    let other_table = tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(other_connection, "second_events"),
        Uuid::now_v7(),
    )
    .await
    .unwrap()
    .id;
    drop(conn);
    let base = chrono::DateTime::from_timestamp_millis(1_767_225_600_000).unwrap();
    record(&pool, &tenant, tenant.table, base, 120).await;
    let mut conn = pool.acquire().await.unwrap();
    samples::record(
        &mut conn,
        tenant.organization,
        other_table,
        &sample(other_connection, base, 900),
    )
    .await
    .unwrap();
    drop(conn);

    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, body) = admin.request("GET", "/api/v1/statistics", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tables"], "2");
    assert_eq!(body["record_count"], "1020");
    assert_eq!(body["data_files"], "6");
    assert!(body["last_sampled_at"].is_string());

    let (status, body) = admin
        .request(
            "GET",
            &format!("/api/v1/catalogs/{}/statistics", tenant.connection),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tables"], "1");
    assert_eq!(body["record_count"], "120");

    // A viewer scoped to one table sees that table alone.
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    let (status, body) = viewer
        .request("GET", "/api/v1/statistics", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tables"], "1");
    assert_eq!(body["record_count"], "120");
    let (status, body) = viewer
        .request(
            "GET",
            &format!("/api/v1/catalogs/{}/statistics", tenant.connection),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // The second connection is invisible to the table-scoped viewer.
    assert_eq!(
        viewer
            .request(
                "GET",
                &format!("/api/v1/catalogs/{other_connection}/statistics"),
                None,
                None
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        admin
            .request(
                "GET",
                &format!("/api/v1/catalogs/{}/statistics", Uuid::now_v7()),
                None,
                None
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}
