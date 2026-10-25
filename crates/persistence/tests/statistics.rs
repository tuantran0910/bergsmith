//! Table statistics storage: append-only samples, retention, and aggregates.

// clippy's `allow-unwrap-in-tests` and `allow-panic-in-tests` cover only
// `#[test]` functions; the helpers fail the test on setup errors.
#![expect(clippy::unwrap_used, reason = "test helpers fail the test")]

mod support;

use bergsmith_common::permission::Role;
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::statistics::{self, NewTableSample};
use bergsmith_persistence::{PersistenceError, tables, tenancy};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::json;
use sqlx::PgPool;

fn sample(connection: bergsmith_common::ids::CatalogConnectionId) -> NewTableSample {
    NewTableSample {
        catalog_connection_id: connection,
        sampled_at: DateTime::from_timestamp_millis(1_767_225_600_000).unwrap(),
        metadata_location: "file:///t/metadata/v1.json".to_owned(),
        snapshot_id: Some(7),
        last_operation: Some("append".to_owned()),
        record_count: 120,
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
        oldest_snapshot_at: Some(DateTime::from_timestamp_millis(1_767_225_000_000).unwrap()),
        newest_snapshot_at: Some(DateTime::from_timestamp_millis(1_767_225_600_000).unwrap()),
        size_histogram: json!([
            {"lower_bound_bytes": 0, "upper_bound_bytes": 1024, "files": 1, "bytes": 512},
            {"lower_bound_bytes": 1024, "upper_bound_bytes": null, "files": 2, "bytes": 3584}
        ]),
        top_partitions: json!([
            {"spec_id": 0, "partition": "day=2026-01-01", "files": 2, "bytes": 2_048}
        ]),
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn samples_are_append_only_and_read_newest_first(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "statistics").await;
    let first = sample(tenant.connection);
    let mut second = first.clone();
    second.sampled_at += TimeDelta::minutes(5);
    second.record_count = 200;
    second.metadata_location = "file:///t/metadata/v2.json".to_owned();

    assert!(
        statistics::record(&mut conn, tenant.organization, tenant.table, &first)
            .await
            .unwrap()
    );
    // The same sample cannot be recorded twice.
    assert!(
        !statistics::record(&mut conn, tenant.organization, tenant.table, &first)
            .await
            .unwrap()
    );
    assert!(
        statistics::record(&mut conn, tenant.organization, tenant.table, &second)
            .await
            .unwrap()
    );

    let latest = statistics::latest(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.record_count, 200);
    assert_eq!(latest.metadata_location, second.metadata_location);
    assert_eq!(latest.size_histogram, second.size_histogram);

    let history = statistics::history(&mut conn, tenant.organization, tenant.table, None, None, 10)
        .await
        .unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].sampled_at, second.sampled_at);
    assert_eq!(history[1].sampled_at, first.sampled_at);

    let before = statistics::history(
        &mut conn,
        tenant.organization,
        tenant.table,
        Some(second.sampled_at),
        None,
        10,
    )
    .await
    .unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].record_count, 120);

    let missing = statistics::latest(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap();
    assert!(missing.is_some());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn retention_prunes_only_samples_older_than_the_bound(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "retention").await;
    let old = sample(tenant.connection);
    let mut recent = sample(tenant.connection);
    recent.sampled_at += TimeDelta::days(100);
    statistics::record(&mut conn, tenant.organization, tenant.table, &old)
        .await
        .unwrap();
    statistics::record(&mut conn, tenant.organization, tenant.table, &recent)
        .await
        .unwrap();

    let pruned = statistics::prune(&mut conn, recent.sampled_at, 10)
        .await
        .unwrap();
    assert_eq!(pruned, 1);
    let remaining =
        statistics::history(&mut conn, tenant.organization, tenant.table, None, None, 10)
            .await
            .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].sampled_at, recent.sampled_at);

    assert!(matches!(
        statistics::prune(&mut conn, recent.sampled_at, 0).await,
        Err(PersistenceError::InvalidValue { .. })
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn due_tables_are_oldest_first_and_stop_when_sampled(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "due").await;
    // PostgreSQL keeps microseconds, so a recorded instant must already be
    // truncated to survive the round trip unchanged.
    let now = DateTime::from_timestamp_micros(Utc::now().timestamp_micros()).unwrap();
    // The caller passes the instant samples older than the interval must beat.
    let cutoff = now - TimeDelta::minutes(5);

    let due = statistics::due_tables(&mut conn, cutoff, 10).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].table_id, tenant.table);
    assert_eq!(due[0].last_sampled_at, None);
    let routes = statistics::maintenance_routes(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].connection_id, tenant.connection);
    assert_eq!(routes[0].namespace, vec!["db".to_owned()]);
    assert_eq!(routes[0].table_name, "events");

    let mut fresh = sample(tenant.connection);
    fresh.sampled_at = now;
    statistics::record(&mut conn, tenant.organization, tenant.table, &fresh)
        .await
        .unwrap();
    let due = statistics::due_tables(&mut conn, cutoff, 10).await.unwrap();
    assert!(due.is_empty());

    // Once the sample is older than the interval, the table is due again.
    let later = now + TimeDelta::minutes(10);
    let due = statistics::due_tables(&mut conn, later, 10).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].last_sampled_at, Some(fresh.sampled_at));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn routes_are_ordered_and_skip_connections_that_may_not_run(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "routes").await;
    let later = support::connection(&mut conn, tenant.organization, "later").await;
    let identity = tables::table_identity(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap();
    tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(later, "events"),
        identity.table_uuid,
    )
    .await
    .unwrap();

    let routes = statistics::maintenance_routes(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap();
    let mut expected = vec![tenant.connection, later];
    expected.sort();
    assert_eq!(
        routes
            .iter()
            .map(|route| route.connection_id)
            .collect::<Vec<_>>(),
        expected
    );

    sqlx::query("UPDATE catalog_connections SET enabled = false WHERE id = $1")
        .bind(later.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let routes = statistics::maintenance_routes(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0].connection_id, tenant.connection);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn paused_connections_are_not_sampled(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "paused").await;
    sqlx::query("UPDATE catalog_connections SET maintenance_enabled = false WHERE id = $1")
        .bind(tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();

    let due = statistics::due_tables(&mut conn, Utc::now(), 10)
        .await
        .unwrap();
    assert!(due.is_empty());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn totals_cover_the_newest_sample_of_visible_tables(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "totals").await;
    let mut older = sample(tenant.connection);
    older.sampled_at -= TimeDelta::hours(1);
    older.record_count = 1_000;
    let newer = sample(tenant.connection);
    statistics::record(&mut conn, tenant.organization, tenant.table, &older)
        .await
        .unwrap();
    statistics::record(&mut conn, tenant.organization, tenant.table, &newer)
        .await
        .unwrap();

    let roles = vec![Role::Administrator.as_str().to_owned()];
    let all = statistics::totals(&mut conn, tenant.organization, tenant.user, &roles, None)
        .await
        .unwrap();
    assert_eq!(all.tables, 1);
    assert_eq!(all.record_count, newer.record_count.cast_signed());
    assert_eq!(all.data_bytes, newer.data_bytes.cast_signed());
    assert_eq!(all.small_files, newer.small_files.cast_signed());
    assert_eq!(all.last_sampled_at, Some(newer.sampled_at));

    // A second table the caller cannot see does not change the totals.
    let other = support::user(&mut conn, tenant.organization, "bob").await;
    let other_connection = support::connection(&mut conn, tenant.organization, "other").await;
    let other_table = tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(other_connection, "other_events"),
        uuid::Uuid::now_v7(),
    )
    .await
    .unwrap()
    .id;
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: bergsmith_common::ids::RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: other,
            role: Role::Administrator,
            scope: bergsmith_common::permission::GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    let mut other_sample = sample(other_connection);
    other_sample.record_count = 9_999;
    statistics::record(&mut conn, tenant.organization, other_table, &other_sample)
        .await
        .unwrap();

    let scoped = statistics::totals(
        &mut conn,
        tenant.organization,
        other,
        &roles,
        Some(other_connection),
    )
    .await
    .unwrap();
    assert_eq!(scoped.tables, 1);
    assert_eq!(scoped.record_count, 9_999);

    // The first caller only sees the first table, on any connection.
    let first = statistics::totals(
        &mut conn,
        tenant.organization,
        tenant.user,
        &roles,
        Some(tenant.connection),
    )
    .await
    .unwrap();
    assert_eq!(first.tables, 1);
    assert_eq!(first.record_count, newer.record_count.cast_signed());
    assert!(
        tenancy::catalog_connection(&mut conn, tenant.organization, tenant.connection)
            .await
            .is_ok()
    );
}
