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
use sqlx::{Connection, PgPool};
use uuid::Uuid;

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
        data_manifest_count: 2,
        manifest_bytes: 2_048,
        partition_count: 2,
        max_partition_files: 2,
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
async fn batch_reads_take_the_newest_sample_and_check_per_table(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "statistics").await;
    let unsampled = tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(tenant.connection, "archive"),
        Uuid::now_v7(),
    )
    .await
    .unwrap()
    .id;
    let other = support::tenant(&mut conn, "other").await;
    let first = sample(tenant.connection);
    let mut second = first.clone();
    second.sampled_at += TimeDelta::minutes(5);
    second.record_count = 200;
    assert!(
        statistics::record(&mut conn, tenant.organization, tenant.table, &first)
            .await
            .unwrap()
    );
    assert!(
        statistics::record(&mut conn, tenant.organization, tenant.table, &second)
            .await
            .unwrap()
    );
    let checked_at = second.sampled_at + TimeDelta::minutes(1);
    statistics::record_check(&mut conn, tenant.organization, tenant.table, checked_at)
        .await
        .unwrap();

    // The newest sample of each sampled table, and nothing for an unsampled
    // one or a table outside the organization.
    let latest =
        statistics::latest_for_tables(&mut conn, tenant.organization, &[tenant.table, unsampled])
            .await
            .unwrap();
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].0, tenant.table);
    assert_eq!(latest[0].1.record_count, 200);
    assert_eq!(latest[0].1.sampled_at, second.sampled_at);
    assert!(
        statistics::latest_for_tables(&mut conn, other.organization, &[tenant.table])
            .await
            .unwrap()
            .is_empty()
    );

    // Confirmations are read for the same set, and an unchecked table is
    // absent rather than reported as stale.
    assert_eq!(
        statistics::checks_for_tables(&mut conn, tenant.organization, &[tenant.table, unsampled])
            .await
            .unwrap(),
        vec![(tenant.table, checked_at)]
    );
    assert!(
        statistics::checks_for_tables(&mut conn, tenant.organization, &[])
            .await
            .unwrap()
            .is_empty()
    );
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
    assert_eq!(due[0].last_confirmed_at, None);
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
    assert_eq!(due[0].last_confirmed_at, Some(fresh.sampled_at));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn samples_record_the_counter_version_they_carry(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "version").await;
    statistics::record(
        &mut conn,
        tenant.organization,
        tenant.table,
        &sample(tenant.connection),
    )
    .await
    .unwrap();
    let latest = statistics::latest(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.sample_version, statistics::SAMPLE_VERSION);
    assert!(latest.data_manifest_count > 0);
    assert!(latest.max_partition_files > 0);

    // A sample written before the counters existed reads back as version 0.
    sqlx::query("UPDATE table_statistics SET sample_version = 0")
        .execute(&mut *conn)
        .await
        .unwrap();
    let latest = statistics::latest(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.sample_version, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn confirmations_keep_an_unchanged_table_fresh_and_move_forward_only(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "confirmed").await;
    let now = DateTime::from_timestamp_micros(Utc::now().timestamp_micros()).unwrap();
    let cutoff = now - TimeDelta::minutes(5);

    // Neither sampled nor checked: due.
    let due = statistics::due_tables(&mut conn, cutoff, 10).await.unwrap();
    assert_eq!(due.len(), 1);

    // A confirmation one minute ago is fresh even though no sample exists.
    let checked = now - TimeDelta::minutes(1);
    statistics::record_check(&mut conn, tenant.organization, tenant.table, checked)
        .await
        .unwrap();
    assert_eq!(
        statistics::last_confirmed_at(&mut conn, tenant.organization, tenant.table)
            .await
            .unwrap(),
        Some(checked)
    );
    assert!(
        statistics::due_tables(&mut conn, cutoff, 10)
            .await
            .unwrap()
            .is_empty()
    );

    // A retried, older confirmation does not move the instant back.
    statistics::record_check(
        &mut conn,
        tenant.organization,
        tenant.table,
        checked - TimeDelta::hours(1),
    )
    .await
    .unwrap();
    assert_eq!(
        statistics::last_confirmed_at(&mut conn, tenant.organization, tenant.table)
            .await
            .unwrap(),
        Some(checked)
    );

    // Once the interval has passed since the confirmation, the table is due.
    let due = statistics::due_tables(&mut conn, checked, 10)
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].last_confirmed_at, Some(checked));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn freshness_falls_back_to_the_newest_sample(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "freshness").await;
    let mut recorded = sample(tenant.connection);
    recorded.sampled_at = DateTime::from_timestamp_micros(Utc::now().timestamp_micros()).unwrap();
    statistics::record(&mut conn, tenant.organization, tenant.table, &recorded)
        .await
        .unwrap();
    assert_eq!(
        statistics::last_confirmed_at(&mut conn, tenant.organization, tenant.table)
            .await
            .unwrap(),
        Some(recorded.sampled_at)
    );

    // A later confirmation is the freshness even though the sample is older.
    let checked = recorded.sampled_at + TimeDelta::minutes(5);
    statistics::record_check(&mut conn, tenant.organization, tenant.table, checked)
        .await
        .unwrap();
    assert_eq!(
        statistics::last_confirmed_at(&mut conn, tenant.organization, tenant.table)
            .await
            .unwrap(),
        Some(checked)
    );
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
        Uuid::now_v7(),
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

const HOUR_MS: i64 = 3_600_000;
const LEDGER_BASE_MS: i64 = 1_767_225_600_000;

fn snapshot(
    connection: bergsmith_common::ids::CatalogConnectionId,
    id: i64,
    at_ms: i64,
) -> statistics::NewTableSnapshot {
    statistics::NewTableSnapshot {
        catalog_connection_id: connection,
        snapshot_id: id,
        parent_snapshot_id: (id > 1).then_some(id - 1),
        sequence_number: id,
        committed_at: DateTime::from_timestamp_millis(at_ms).unwrap(),
        operation: "append".to_owned(),
        summary: json!({"added-data-files": "1", "bergsmith.job-id": "job"}),
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn snapshots_are_recorded_once_and_read_newest_first(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "snapshots").await;
    let observed = DateTime::from_timestamp_millis(LEDGER_BASE_MS).unwrap();
    let first = snapshot(tenant.connection, 1, LEDGER_BASE_MS);
    let second = snapshot(tenant.connection, 2, LEDGER_BASE_MS + 60_000);
    let third = snapshot(tenant.connection, 3, LEDGER_BASE_MS + 120_000);

    assert!(
        !statistics::has_snapshot(&mut conn, tenant.organization, tenant.table, 1)
            .await
            .unwrap()
    );
    let inserted = statistics::record_snapshots(
        &mut conn,
        tenant.organization,
        tenant.table,
        &[first.clone(), second.clone()],
        observed,
    )
    .await
    .unwrap();
    assert_eq!(inserted, 2);
    // A snapshot is never rewritten: a repeated pass inserts nothing.
    let repeated = statistics::record_snapshots(
        &mut conn,
        tenant.organization,
        tenant.table,
        &[first.clone(), second.clone()],
        observed,
    )
    .await
    .unwrap();
    assert_eq!(repeated, 0);
    assert!(
        statistics::has_snapshot(&mut conn, tenant.organization, tenant.table, 2)
            .await
            .unwrap()
    );
    statistics::record_snapshots(
        &mut conn,
        tenant.organization,
        tenant.table,
        std::slice::from_ref(&third),
        observed,
    )
    .await
    .unwrap();

    let timeline =
        statistics::snapshots(&mut conn, tenant.organization, tenant.table, None, None, 10)
            .await
            .unwrap();
    assert_eq!(timeline.len(), 3);
    assert_eq!(timeline[0].snapshot_id, 3);
    assert_eq!(timeline[0].parent_snapshot_id, Some(2));
    assert_eq!(timeline[0].operation, "append");
    assert_eq!(timeline[0].summary["added-data-files"], "1");
    assert_eq!(timeline[2].parent_snapshot_id, None);

    let after = statistics::snapshots(
        &mut conn,
        tenant.organization,
        tenant.table,
        Some((third.committed_at, third.snapshot_id)),
        None,
        10,
    )
    .await
    .unwrap();
    assert_eq!(after.len(), 2);
    assert_eq!(after[0].snapshot_id, 2);

    let since = statistics::snapshots(
        &mut conn,
        tenant.organization,
        tenant.table,
        None,
        Some(second.committed_at),
        10,
    )
    .await
    .unwrap();
    assert_eq!(since.len(), 2);
    assert_eq!(since[1].snapshot_id, 2);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn snapshot_retention_prunes_by_observation(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "snapshot-retention").await;
    let observed = DateTime::from_timestamp_millis(LEDGER_BASE_MS).unwrap();
    statistics::record_snapshots(
        &mut conn,
        tenant.organization,
        tenant.table,
        &[snapshot(tenant.connection, 1, LEDGER_BASE_MS)],
        observed,
    )
    .await
    .unwrap();
    statistics::record_snapshots(
        &mut conn,
        tenant.organization,
        tenant.table,
        &[snapshot(tenant.connection, 2, LEDGER_BASE_MS + 60_000)],
        observed + TimeDelta::days(100),
    )
    .await
    .unwrap();

    let pruned = statistics::prune_snapshots(&mut conn, observed + TimeDelta::days(100), 10)
        .await
        .unwrap();
    assert_eq!(pruned, 1);
    let remaining =
        statistics::snapshots(&mut conn, tenant.organization, tenant.table, None, None, 10)
            .await
            .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].snapshot_id, 2);

    assert!(matches!(
        statistics::prune_snapshots(&mut conn, observed, 0).await,
        Err(PersistenceError::InvalidValue { .. })
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn commit_hours_bucket_commits_in_utc(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "commit-hours").await;
    let observed = DateTime::from_timestamp_millis(LEDGER_BASE_MS).unwrap();
    let entries = [
        snapshot(tenant.connection, 1, LEDGER_BASE_MS - 60_000),
        snapshot(tenant.connection, 2, LEDGER_BASE_MS + 5 * 60_000),
        snapshot(tenant.connection, 3, LEDGER_BASE_MS + 45 * 60_000),
        snapshot(tenant.connection, 4, LEDGER_BASE_MS + HOUR_MS),
    ];
    statistics::record_snapshots(
        &mut conn,
        tenant.organization,
        tenant.table,
        &entries,
        observed,
    )
    .await
    .unwrap();

    let hours = statistics::commit_hours(
        &mut conn,
        tenant.organization,
        tenant.table,
        observed - TimeDelta::hours(1),
        observed + TimeDelta::hours(2),
    )
    .await
    .unwrap();
    assert_eq!(hours.len(), 3);
    assert_eq!(hours[0].hour, observed - TimeDelta::hours(1));
    assert_eq!(hours[0].commits, 1);
    assert_eq!(hours[1].hour, observed);
    assert_eq!(hours[1].commits, 2);
    assert_eq!(hours[2].hour, observed + TimeDelta::hours(1));
    assert_eq!(hours[2].commits, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_shared_transaction_rolls_back_a_sample_with_its_snapshots(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "atomic").await;
    let foreign = support::tenant(&mut conn, "atomic-other").await;
    let observed = DateTime::from_timestamp_millis(LEDGER_BASE_MS).unwrap();
    // The collector writes both rows in one transaction; a failed ledger
    // insert must leave no sample behind for the next pass to mistake for
    // current.
    let mut tx = conn.begin().await.unwrap();
    statistics::record(
        &mut tx,
        tenant.organization,
        tenant.table,
        &sample(tenant.connection),
    )
    .await
    .unwrap();
    let mut refused = snapshot(tenant.connection, 1, LEDGER_BASE_MS);
    refused.catalog_connection_id = foreign.connection;
    assert!(matches!(
        statistics::record_snapshots(
            &mut tx,
            tenant.organization,
            tenant.table,
            &[refused],
            observed
        )
        .await,
        Err(PersistenceError::MissingReference { .. })
    ));
    tx.rollback().await.unwrap();

    assert!(
        statistics::latest(&mut conn, tenant.organization, tenant.table)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !statistics::has_snapshot(&mut conn, tenant.organization, tenant.table, 1)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_rolled_back_snapshot_is_recognized_beside_a_newer_one(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "rolled-back").await;
    let observed = DateTime::from_timestamp_millis(LEDGER_BASE_MS).unwrap();
    statistics::record_snapshots(
        &mut conn,
        tenant.organization,
        tenant.table,
        &[
            snapshot(tenant.connection, 1, LEDGER_BASE_MS),
            snapshot(tenant.connection, 2, LEDGER_BASE_MS + 60_000),
        ],
        observed,
    )
    .await
    .unwrap();

    // A rollback points the table at the first commit while the ledger still
    // holds the second, so the newest row says nothing about the current one.
    for snapshot_id in [1, 2] {
        assert!(
            statistics::has_snapshot(&mut conn, tenant.organization, tenant.table, snapshot_id)
                .await
                .unwrap(),
            "snapshot {snapshot_id}"
        );
    }
    assert!(
        !statistics::has_snapshot(&mut conn, tenant.organization, tenant.table, 3)
            .await
            .unwrap()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn snapshots_sharing_an_instant_are_both_recognized(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "same-instant").await;
    let observed = DateTime::from_timestamp_millis(LEDGER_BASE_MS).unwrap();
    statistics::record_snapshots(
        &mut conn,
        tenant.organization,
        tenant.table,
        &[
            snapshot(tenant.connection, 6, LEDGER_BASE_MS),
            snapshot(tenant.connection, 7, LEDGER_BASE_MS),
        ],
        observed,
    )
    .await
    .unwrap();

    for snapshot_id in [6, 7] {
        assert!(
            statistics::has_snapshot(&mut conn, tenant.organization, tenant.table, snapshot_id)
                .await
                .unwrap(),
            "snapshot {snapshot_id}"
        );
    }
}
