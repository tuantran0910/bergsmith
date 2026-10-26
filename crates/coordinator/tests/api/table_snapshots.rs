//! Snapshot timeline contracts (BS-026.T1).
//!
//! The ledger rows are inserted through the repository; these tests cover the
//! HTTP contract, the cursor, the cadence window, and table-scoped access.

use super::*;

use bergsmith_persistence::statistics as ledger;
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde_json::json;
use uuid::Uuid;

/// The start of the hour `at` falls in.
fn hour_start(at: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp(at.timestamp().div_euclid(3_600) * 3_600, 0).unwrap()
}

/// A bucket label as the API stamps it.
fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// One commit; `job` adds Bergsmith's publication evidence to its summary.
fn snapshot(
    connection: CatalogConnectionId,
    id: i64,
    at: DateTime<Utc>,
    job: Option<Uuid>,
) -> ledger::NewTableSnapshot {
    let mut summary = json!({
        "added-data-files": "2",
        "added-records": "40",
        "changed-partition-count": "1",
        "engine-name": "spark",
        "engine-version": "4.0.0",
        // Iceberg writes per-partition counters under their partition path,
        // which is the partition value itself.
        "partitions.day=2026-01-01.records": "40",
    });
    if let Some(job) = job {
        summary["bergsmith.job-id"] = json!(job.to_string());
        summary["bergsmith.publication-id"] = json!(Uuid::now_v7().to_string());
    }
    ledger::NewTableSnapshot {
        catalog_connection_id: connection,
        snapshot_id: id,
        parent_snapshot_id: (id > 1).then_some(id - 1),
        sequence_number: id,
        committed_at: at,
        operation: "append".to_owned(),
        summary,
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn snapshots_expose_commits_deltas_and_the_cadence(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "snapshots").await;
    let foreign = support::tenant(&mut conn, "foreign-snapshots").await;
    let job = Uuid::now_v7();
    let base = hour_start(Utc::now()) - TimeDelta::hours(2);
    ledger::record_snapshots(
        &mut conn,
        tenant.organization,
        tenant.table,
        &[
            snapshot(tenant.connection, 1, base, None),
            snapshot(tenant.connection, 2, base + TimeDelta::minutes(5), None),
            snapshot(tenant.connection, 3, base + TimeDelta::hours(1), Some(job)),
        ],
        Utc::now(),
    )
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let uri = format!("/api/v1/tables/{}/snapshots", tenant.table);
    let (status, body) = browser.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["snapshot_id"], "3");
    assert_eq!(items[0]["parent_snapshot_id"], "2");
    assert_eq!(items[0]["sequence_number"], "3");
    assert_eq!(items[0]["operation"], "append");
    assert_eq!(items[0]["delta"]["added_data_files"], "2");
    assert_eq!(items[0]["delta"]["added_records"], "40");
    assert_eq!(items[0]["delta"]["deleted_data_files"], "0");
    assert_eq!(items[0]["delta"]["added_delete_files"], "0");
    assert_eq!(items[0]["job_id"], job.to_string());
    assert_eq!(items[0]["properties"]["engine-name"], "spark");
    assert_eq!(items[0]["properties"]["bergsmith.job-id"], job.to_string());
    // A principal allowed to plan sees the partition counters.
    assert_eq!(
        items[0]["properties"]["partitions.day=2026-01-01.records"],
        "40"
    );
    assert_eq!(items[1]["job_id"], Value::Null);
    assert_eq!(items[2]["parent_snapshot_id"], Value::Null);

    // The chart ends with the current hour and counts the recorded commits.
    let cadence = &body["cadence"];
    assert_eq!(cadence["hours"], 24);
    let buckets = cadence["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 24);
    let counts: Vec<(&str, &str)> = buckets
        .iter()
        .map(|bucket| {
            (
                bucket["start"].as_str().unwrap(),
                bucket["commits"].as_str().unwrap(),
            )
        })
        .collect();
    let first = stamp(base);
    let second = stamp(base + TimeDelta::hours(1));
    assert!(counts.contains(&(first.as_str(), "2")));
    assert!(counts.contains(&(second.as_str(), "1")));
    assert_eq!(
        buckets.last().unwrap()["start"],
        stamp(hour_start(Utc::now()))
    );

    // A window that is not a day long still returns one bucket per hour.
    let (status, body) = browser
        .request("GET", &format!("{uri}?hours=3"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["cadence"]["hours"], 3);
    assert_eq!(body["cadence"]["buckets"].as_array().unwrap().len(), 3);
    for hours in ["0", "169", "many"] {
        assert_eq!(
            browser
                .request("GET", &format!("{uri}?hours={hours}"), None, None)
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "hours={hours}"
        );
    }

    // The cursor continues strictly after the last returned commit.
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
    assert_eq!(next["items"][0]["snapshot_id"], "2");
    assert!(next["next_cursor"].is_string());

    // An inclusive lower bound hides older commits but not the chart.
    let since = stamp(base + TimeDelta::hours(1));
    let (status, body) = browser
        .request("GET", &format!("{uri}?since={since}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["snapshot_id"], "3");
    assert_eq!(
        body["cadence"]["buckets"].as_array().unwrap().len(),
        24,
        "the cadence window is not narrowed by the timeline filter"
    );

    // A viewer scoped to the table reads its commits.
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    let (status, body) = viewer.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["job_id"], job.to_string());
    // A viewer sees the commits and their counts but no partition values,
    // which are the same metadata a preview gates on `jobs:submit`.
    let properties = body["items"][0]["properties"].as_object().unwrap();
    assert!(
        properties.keys().all(|key| !key.starts_with("partitions.")),
        "a partition counter leaked: {properties:?}"
    );
    assert_eq!(body["items"][0]["properties"]["engine-name"], "spark");
    assert_eq!(
        body["items"][0]["properties"]["changed-partition-count"],
        "1"
    );

    // Another organization cannot tell the table from a missing one.
    let foreign_browser = Browser::new(&pool, &foreign, foreign.user, "alice").await;
    assert_eq!(
        foreign_browser.request("GET", &uri, None, None).await.0,
        StatusCode::NOT_FOUND
    );
    let (status, body) = browser
        .request(
            "GET",
            &format!("/api/v1/tables/{}/snapshots", Uuid::now_v7()),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["reason"], "resource_not_found");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unsampled_table_has_an_empty_timeline(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "unsampled").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, body) = browser
        .request(
            "GET",
            &format!("/api/v1/tables/{}/snapshots", tenant.table),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["items"].as_array().unwrap().is_empty());
    assert!(body["next_cursor"].is_null());
    assert_eq!(body["cadence"]["buckets"].as_array().unwrap().len(), 24);
    assert!(
        body["cadence"]["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|bucket| bucket["commits"] == "0")
    );
}
