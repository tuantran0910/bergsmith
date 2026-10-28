//! Metrics exposition against a real ledger (BS-020.T2).

#![allow(clippy::unwrap_used, reason = "fixtures fail the test immediately")]

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bergsmith_coordinator::observability::{self, MetricsConfig, Observability};
use bergsmith_persistence::Database;
use sqlx::PgPool;
use tower::ServiceExt as _;

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn metrics_are_served_from_a_cached_ledger_snapshot(pool: PgPool) {
    let observability = Observability::new(
        Database::from_pool(pool.clone()),
        MetricsConfig {
            refresh: Duration::from_secs(3600),
            query_timeout: Duration::from_secs(5),
            ..MetricsConfig::default()
        },
    );
    let router = observability::router(observability.clone());

    let response = router
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/plain; version=0.0.4")
    );
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("bergsmith_ledger_up 1\n"));
    assert!(text.contains("bergsmith_jobs_active{state=\"Queued\"} 0\n"));
    assert!(text.contains("bergsmith_tasks_pending 0\n"));

    // Within the refresh interval the cached snapshot is served, so a closed
    // pool does not fail the scrape.
    pool.close().await;
    let cached = observability.render().await;
    assert!(cached.contains("bergsmith_ledger_up 1\n"));
    assert!(cached.contains("bergsmith_tasks_pending 0\n"));
}

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

use bergsmith_common::ids::CatalogConnectionId;
use bergsmith_persistence::{statistics, tables};
use chrono::{DateTime, Utc};
use serde_json::json;

fn sample(connection: CatalogConnectionId, at: DateTime<Utc>) -> statistics::NewTableSample {
    statistics::NewTableSample {
        catalog_connection_id: connection,
        sampled_at: at,
        metadata_location: "s3://private/location/metadata.json".into(),
        snapshot_id: Some(7),
        last_operation: Some("append".into()),
        record_count: 120,
        data_files: 3,
        data_bytes: 600 * 1024 * 1024,
        delete_files: 0,
        delete_bytes: 0,
        position_delete_files: 0,
        equality_delete_files: 0,
        position_delete_records: 0,
        equality_delete_records: 0,
        manifest_count: 2,
        data_manifest_count: 2,
        manifest_bytes: 2048,
        partition_count: 2,
        max_partition_files: 2,
        small_files: 1,
        target_file_size_bytes: 512 * 1024 * 1024,
        snapshot_count: 4,
        oldest_snapshot_at: Some(at),
        newest_snapshot_at: Some(at),
        size_histogram: json!([]),
        top_partitions: json!([]),
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn health_metrics_deduplicate_aliases_and_keep_missing_evidence_unassessed(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "health").await;
    let other = support::tenant(&mut conn, "other").await;
    let empty_connection = support::connection(&mut conn, tenant.organization, "empty").await;
    let second = support::connection(&mut conn, tenant.organization, "second").await;
    let uuid: uuid::Uuid =
        sqlx::query_scalar("SELECT table_uuid FROM table_identities WHERE id = $1")
            .bind(tenant.table.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    for (connection, name) in [(tenant.connection, "alias"), (second, "shared")] {
        tables::resolve_table(
            &mut conn,
            tenant.organization,
            &support::alias(connection, name),
            uuid,
        )
        .await
        .unwrap();
    }
    let at: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    statistics::record(
        &mut conn,
        tenant.organization,
        tenant.table,
        &sample(tenant.connection, at),
    )
    .await
    .unwrap();
    let mut older = sample(tenant.connection, at - chrono::TimeDelta::days(1));
    older.data_files = 999;
    statistics::record(&mut conn, tenant.organization, tenant.table, &older)
        .await
        .unwrap();
    drop(conn);
    let metrics = Observability::new(Database::from_pool(pool), MetricsConfig::default());
    let (text, concurrent) = tokio::join!(metrics.render(), metrics.render());
    assert!(text.contains("bergsmith_tables_health{state=\"healthy\"} 1\n"));
    assert!(concurrent.contains("bergsmith_tables_health{state=\"unassessed\"} 1\n"));
    assert!(text.contains("bergsmith_statistics_data_files 3\n"));
    for connection in [tenant.connection, second] {
        assert!(text.contains(&format!(
            "bergsmith_connection_statistics_data_files{{connection_id=\"{connection}\"}} 3\n"
        )));
    }
    for connection in [empty_connection, other.connection] {
        assert!(text.contains(&format!(
            "bergsmith_connection_statistics_data_files{{connection_id=\"{connection}\"}} 0\n"
        )));
    }
    assert!(!text.contains("table_id="));
    for forbidden in [
        "private/location",
        "catalog.example",
        "secret/catalog",
        "events",
        "warehouse",
    ] {
        assert!(!text.contains(forbidden));
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn metrics_reassess_freshness_and_legacy_samples_at_refresh(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "stale").await;
    let at: DateTime<Utc> = sqlx::query_scalar("SELECT now() - interval '7 hours'")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    statistics::record(
        &mut conn,
        tenant.organization,
        tenant.table,
        &sample(tenant.connection, at),
    )
    .await
    .unwrap();
    drop(conn);
    let metrics = Observability::new(
        Database::from_pool(pool.clone()),
        MetricsConfig {
            refresh: Duration::ZERO,
            per_table_metrics: true,
            ..MetricsConfig::default()
        },
    );
    let text = metrics.render().await;
    assert!(text.contains("bergsmith_tables_health{state=\"critical\"} 1\n"));
    assert!(text.contains(&format!(
        "bergsmith_table_health{{table_id=\"{}\"}} 2\n",
        tenant.table
    )));
    let mut conn = pool.acquire().await.unwrap();
    statistics::record_check(&mut conn, tenant.organization, tenant.table, Utc::now())
        .await
        .unwrap();
    drop(conn);
    assert!(
        metrics
            .render()
            .await
            .contains("bergsmith_tables_health{state=\"healthy\"} 1\n")
    );
    sqlx::query("UPDATE table_statistics SET sample_version = 0")
        .execute(&pool)
        .await
        .unwrap();
    let text = metrics.render().await;
    assert!(text.contains("bergsmith_tables_health{state=\"unassessed\"} 1\n"));
    assert!(text.contains("bergsmith_statistics_data_files 0\n"));
    assert!(!text.contains("bergsmith_table_statistics_data_files{table_id="));
    // A failed refresh exposes only the complete previous snapshot, marked down.
    pool.close().await;
    let text = metrics.render().await;
    assert!(text.contains("bergsmith_ledger_up 0\n"));
    assert!(text.contains("bergsmith_tables_health{state=\"unassessed\"} 1\n"));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn individual_metrics_are_capped_without_truncating_aggregate_coverage(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "capped").await;
    let ids: Vec<_> = (0..260_u128)
        .map(|n| uuid::Uuid::from_u128(n + 100))
        .collect();
    sqlx::query("INSERT INTO table_identities (id, organization_id, table_uuid) SELECT id, $1, id FROM unnest($2::uuid[]) id")
        .bind(tenant.organization.as_uuid()).bind(&ids).execute(&mut *conn).await.unwrap();
    sqlx::query("INSERT INTO table_aliases (catalog_connection_id, namespace, table_name, organization_id, table_identity_id) SELECT $1, ARRAY['db'], id::text, $2, id FROM unnest($3::uuid[]) id")
        .bind(tenant.connection.as_uuid()).bind(tenant.organization.as_uuid()).bind(&ids)
        .execute(&mut *conn).await.unwrap();
    // A later organization contains the globally lowest table UUID. Tenant-
    // ordered scanning must replace an earlier selection rather than keep it.
    let other = support::tenant(&mut conn, "capped-other").await;
    let oldest = uuid::Uuid::from_u128(1);
    sqlx::query(
        "INSERT INTO table_identities (id, organization_id, table_uuid) VALUES ($1, $2, $1)",
    )
    .bind(oldest)
    .bind(other.organization.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("INSERT INTO table_aliases (catalog_connection_id, namespace, table_name, organization_id, table_identity_id) VALUES ($1, ARRAY['db'], 'oldest', $2, $3)")
        .bind(other.connection.as_uuid()).bind(other.organization.as_uuid()).bind(oldest)
        .execute(&mut *conn).await.unwrap();
    drop(conn);
    let metrics = Observability::new(
        Database::from_pool(pool),
        MetricsConfig {
            per_table_metrics: true,
            max_table_metrics: 2,
            ..MetricsConfig::default()
        },
    );
    let text = metrics.render().await;
    assert!(text.contains("bergsmith_tables_health{state=\"unassessed\"} 263\n"));
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("bergsmith_table_health{"))
            .count(),
        2
    );
    assert!(text.contains("bergsmith_table_metrics_omitted 261\n"));
    assert!(text.contains(&format!(
        "bergsmith_table_health{{table_id=\"{}\"}} 3\n",
        ids[0]
    )));
    assert!(text.contains(&format!(
        "bergsmith_table_health{{table_id=\"{oldest}\"}} 3\n"
    )));
}

async fn strict_schedule(conn: &mut sqlx::PgConnection, tenant: &support::Tenant) {
    use bergsmith_common::health::{HealthThresholds, Thresholds};
    use bergsmith_common::ids::{PolicyId, RoleGrantId, ScheduleId};
    use bergsmith_common::permission::{GrantScope, Role};
    use bergsmith_common::policy::PolicySpec;
    use bergsmith_common::schedule::CronSchedule;
    use bergsmith_persistence::{grants, schedules};
    let service = support::service_principal(conn, tenant.organization, "scheduler").await;
    grants::create_grant(
        conn,
        &grants::NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: service,
            role: Role::MaintenanceService,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    let policy = PolicyId::generate();
    schedules::create_policy_with_health(
        conn,
        &schedules::NewMaintenancePolicy {
            id: policy,
            organization_id: tenant.organization,
            name: "strict".into(),
            specification: PolicySpec::default(),
            created_by: tenant.user,
        },
        &HealthThresholds {
            small_files: Thresholds {
                warning: 2_147_483_648,
                critical: 1_073_741_824,
            },
            ..HealthThresholds::default()
        },
        None,
    )
    .await
    .unwrap();
    schedules::create_schedule(
        conn,
        &schedules::NewMaintenanceSchedule {
            id: ScheduleId::generate(),
            organization_id: tenant.organization,
            policy_id: policy,
            table_id: tenant.table,
            timing: CronSchedule::parse("0 2 * * *", "UTC").unwrap(),
            window: None,
            service_principal_id: service,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn metrics_follow_enabled_policy_edits_without_a_new_sample(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "policy").await;
    statistics::record(
        &mut conn,
        tenant.organization,
        tenant.table,
        &sample(tenant.connection, Utc::now()),
    )
    .await
    .unwrap();
    strict_schedule(&mut conn, &tenant).await;
    drop(conn);
    let metrics = Observability::new(
        Database::from_pool(pool.clone()),
        MetricsConfig {
            refresh: Duration::ZERO,
            ..MetricsConfig::default()
        },
    );
    assert!(
        metrics
            .render()
            .await
            .contains("bergsmith_tables_health{state=\"critical\"} 1\n")
    );
    sqlx::query("UPDATE maintenance_policies SET enabled = false")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        metrics
            .render()
            .await
            .contains("bergsmith_tables_health{state=\"healthy\"} 1\n")
    );
    sqlx::query("UPDATE maintenance_policies SET enabled = true, health_thresholds = '{}'::jsonb")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        metrics
            .render()
            .await
            .contains("bergsmith_tables_health{state=\"healthy\"} 1\n")
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_health_timeout_does_not_freeze_publication_metrics(pool: PgPool) {
    use bergsmith_common::job::JobState;
    use sqlx::Connection as _;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "timeout").await;
    let job = support::job(&mut conn, &tenant).await;
    statistics::record(
        &mut conn,
        tenant.organization,
        tenant.table,
        &sample(tenant.connection, Utc::now()),
    )
    .await
    .unwrap();
    drop(conn);
    let metrics = Observability::new(
        Database::from_pool(pool.clone()),
        MetricsConfig {
            refresh: Duration::ZERO,
            // Only the health read may time out; a loaded run must not push
            // the ledger read past its own deadline.
            query_timeout: Duration::from_secs(30),
            health_query_timeout: Duration::from_secs(2),
            ..MetricsConfig::default()
        },
    );
    let before = metrics.render().await;
    assert!(before.contains("bergsmith_health_metrics_up 1\n"));
    assert!(before.contains("bergsmith_publications_unresolved 0\n"));
    let mut blocker = pool.acquire().await.unwrap();
    support::force_job_state(&mut blocker, job, JobState::CommitOutcomeUnknown).await;
    sqlx::query(
        "UPDATE jobs SET publication_started_at = now() - interval '2 hours' WHERE id = $1",
    )
    .bind(job.as_uuid())
    .execute(&mut *blocker)
    .await
    .unwrap();
    sqlx::query("UPDATE table_statistics SET sample_version = 0")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let mut tx = blocker.begin().await.unwrap();
    sqlx::query("LOCK TABLE catalog_connections IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .unwrap();
    let text = metrics.render().await;
    assert!(text.contains("bergsmith_ledger_up 1\n"));
    assert!(text.contains("bergsmith_publications_unresolved 1\n"));
    assert!(text.contains("bergsmith_health_metrics_up 0\n"));
    assert!(text.contains("bergsmith_health_metrics_snapshot_age_seconds "));
    assert!(text.contains("bergsmith_tables_health{state=\"healthy\"} 1\n"));
    tx.rollback().await.unwrap();
    let text = metrics.render().await;
    assert!(text.contains("bergsmith_health_metrics_up 1\n"));
    assert!(text.contains("bergsmith_tables_health{state=\"unassessed\"} 1\n"));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn unregistering_a_stale_table_removes_its_health_and_statistics(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "unregistered").await;
    let at: DateTime<Utc> = sqlx::query_scalar("SELECT now() - interval '7 hours'")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    statistics::record(
        &mut conn,
        tenant.organization,
        tenant.table,
        &sample(tenant.connection, at),
    )
    .await
    .unwrap();
    drop(conn);
    let metrics = Observability::new(
        Database::from_pool(pool.clone()),
        MetricsConfig {
            refresh: Duration::ZERO,
            per_table_metrics: true,
            ..MetricsConfig::default()
        },
    );
    assert!(
        metrics
            .render()
            .await
            .contains("bergsmith_tables_health{state=\"critical\"} 1\n")
    );
    let mut conn = pool.acquire().await.unwrap();
    let revision: i64 =
        sqlx::query_scalar("SELECT revision FROM table_aliases WHERE table_identity_id = $1")
            .bind(tenant.table.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    tables::unregister_alias(
        &mut conn,
        tenant.organization,
        &support::alias(tenant.connection, "events"),
        revision,
    )
    .await
    .unwrap();
    // Both historical identity and sample deliberately remain in the ledger.
    assert!(
        statistics::latest(&mut conn, tenant.organization, tenant.table)
            .await
            .unwrap()
            .is_some()
    );
    drop(conn);
    let text = metrics.render().await;
    for state in ["healthy", "warning", "critical", "unassessed"] {
        assert!(text.contains(&format!("bergsmith_tables_health{{state=\"{state}\"}} 0\n")));
    }
    assert!(text.contains("bergsmith_statistics_data_files 0\n"));
    assert!(!text.contains(&format!("table_id=\"{}\"", tenant.table)));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn due_ledger_and_health_refreshes_progress_concurrently(pool: PgPool) {
    use sqlx::Connection as _;
    let metrics = Observability::new(
        Database::from_pool(pool.clone()),
        MetricsConfig {
            query_timeout: Duration::from_secs(30),
            health_query_timeout: Duration::from_secs(30),
            ..MetricsConfig::default()
        },
    );
    let mut blocker = pool.acquire().await.unwrap();
    let mut tx = blocker.begin().await.unwrap();
    sqlx::query("LOCK TABLE plans, catalog_connections IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .unwrap();
    let render = tokio::spawn(async move { metrics.render().await });
    // Both readers must reach their own blocked query before either is freed.
    // This observes concurrency directly, without comparing elapsed durations.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let blocked: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_locks WHERE NOT granted
                 AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
                 AND relation IN ('plans'::regclass, 'catalog_connections'::regclass)",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if blocked == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    let text = render.await.unwrap();
    assert!(text.contains("bergsmith_ledger_up 1\n"));
    assert!(text.contains("bergsmith_health_metrics_up 1\n"));
}
