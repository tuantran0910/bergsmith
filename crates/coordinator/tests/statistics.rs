//! The coordinator samples registered tables from metadata only (BS-025.T1).

// Fixtures fail the test immediately; helper functions are not `#[test]` items,
// so clippy's allow-*-in-tests settings do not cover them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use bergsmith_catalog::{ObservedTable, TableName};
use bergsmith_common::ids::{CatalogConnectionId, JobId, OrganizationId, PrincipalId, RoleGrantId};
use bergsmith_common::job::JobOperation;
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_coordinator::inventory::record_observation;
use bergsmith_coordinator::runtime::catalogs::RuntimeCatalogs;
use bergsmith_coordinator::runtime::config::RuntimeProfiles;
use bergsmith_coordinator::runtime::statistics::{
    SampleOutcome, SampleSummary, SamplingSettings, StatisticsCollector,
};
use bergsmith_fixtures::{Commit, FileSpec, Fixture, TableSpec, row};
use bergsmith_maintenance::StatisticsConfig;
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::jobs::{self, NewJob};
use bergsmith_persistence::statistics;
use bergsmith_persistence::tables::{self, TableAlias};
use bergsmith_persistence::tenancy::{
    self, NewCatalogConnection, NewOrganization, NewPrincipal, PrincipalKind,
};
use bergsmith_persistence::{Database, DatabaseConfig};
use bergsmith_storage::{Location, StorageProfile, StorageProfiles};
use chrono::TimeDelta;
use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
use serde_json::json;
use sqlx::{ConnectOptions, PgPool};

fn id_schema() -> Schema {
    Schema::builder()
        .with_fields([Arc::new(NestedField::required(
            1,
            "id",
            Type::Primitive(PrimitiveType::Long),
        ))])
        .build()
        .unwrap()
}

async fn fixture() -> Fixture {
    let mut commit = Commit::new();
    for index in 0..5_i64 {
        commit = commit.file(FileSpec::data(format!("d{index}"), [row![1 => index]]));
    }
    TableSpec::new(id_schema())
        .commit(commit)
        .generate()
        .await
        .unwrap()
}

/// Two commits, so one snapshot can be missing while an older one remains.
async fn committed_twice() -> Fixture {
    TableSpec::new(id_schema())
        .commit(Commit::new().file(FileSpec::data("d0", [row![1 => 0_i64]])))
        .commit(Commit::new().file(FileSpec::data("d1", [row![1 => 1_i64]])))
        .generate()
        .await
        .unwrap()
}

fn database_url(pool: &PgPool) -> String {
    pool.connect_options().to_url_lossy().to_string()
}

fn settings() -> SamplingSettings {
    SamplingSettings {
        interval_millis: 300_000,
        retention_days: 90,
        page_size: 16,
        max_concurrent_connections: 2,
        sample: StatisticsConfig::default(),
    }
}

/// A collector with the local filesystem profile the fixtures live on.
async fn collector(pool: &PgPool) -> StatisticsCollector {
    let mut config =
        DatabaseConfig::new(bergsmith_persistence::DatabaseUrl::new(database_url(pool)));
    config.max_connections = 1;
    let database = Database::connect(&config).await.unwrap();
    let profiles = RuntimeProfiles {
        default: StorageProfiles::new([StorageProfile::LocalFs]),
        connections: BTreeMap::new(),
    };
    StatisticsCollector::new(
        pool.clone(),
        Arc::new(RuntimeCatalogs::new(database, profiles)),
        settings(),
    )
}

/// Mocks the catalog config and table read for `fixture` under `db.events`.
async fn serve_table(server: &mut mockito::ServerGuard, fixture: &Fixture) {
    let _ = server
        .mock("GET", "/v1/config")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .create_async()
        .await;
    let body = json!({
        "metadata-location": fixture.metadata_location(),
        "metadata": serde_json::to_value(fixture.metadata()).unwrap(),
        "config": {}
    });
    let _ = server
        .mock("GET", "/v1/namespaces/db/tables/events")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body.to_string())
        .create_async()
        .await;
}

/// An organization with one connection, one registered table, and one user.
struct Tenant {
    organization: OrganizationId,
    user: PrincipalId,
    connection: CatalogConnectionId,
    table: bergsmith_common::ids::TableId,
}

async fn tenant(conn: &mut sqlx::PgConnection, uri: &str, fixture: &Fixture) -> Tenant {
    let organization = OrganizationId::generate();
    tenancy::create_organization(
        conn,
        &NewOrganization {
            id: organization,
            slug: format!("acme-{organization}"),
            display_name: "Acme".to_owned(),
        },
    )
    .await
    .unwrap();
    let connection = CatalogConnectionId::generate();
    tenancy::create_catalog_connection(
        conn,
        &NewCatalogConnection {
            id: connection,
            organization_id: organization,
            name: "main".to_owned(),
            uri: uri.to_owned(),
            warehouse: None,
            credential_ref: None,
        },
    )
    .await
    .unwrap();
    let observed = ObservedTable {
        name: TableName {
            namespace: vec!["db".to_owned()],
            name: "events".to_owned(),
        },
        table_uuid: fixture.metadata().uuid(),
        location: Location::parse(fixture.metadata().location()).unwrap(),
    };
    let table = record_observation(conn, organization, connection, &observed)
        .await
        .unwrap();
    let user = PrincipalId::generate();
    tenancy::create_principal(
        conn,
        &NewPrincipal {
            id: user,
            organization_id: organization,
            kind: PrincipalKind::User,
            issuer: "https://idp.example".to_owned(),
            subject: format!("sampler-{user}"),
            display_name: "Sampler".to_owned(),
        },
    )
    .await
    .unwrap();
    grants::create_grant(
        conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: organization,
            principal_id: user,
            role: Role::Administrator,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    Tenant {
        organization,
        user,
        connection,
        table,
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_pass_samples_a_table_and_records_nothing_unchanged(pool: PgPool) {
    let fixture = fixture().await;
    let mut server = mockito::Server::new_async().await;
    serve_table(&mut server, &fixture).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, &server.url(), &fixture).await;
    drop(conn);
    let collector = collector(&pool).await;
    // PostgreSQL keeps microseconds, so the instants the test compares must
    // already be truncated or a same-instant assertion can differ by nanos.
    let now = chrono::DateTime::from_timestamp_micros(chrono::Utc::now().timestamp_micros())
        .expect("the current instant");

    let summary: SampleSummary = collector.pass_at(now).await.unwrap();
    assert_eq!(
        summary,
        SampleSummary {
            sampled: 1,
            unchanged: 0,
            skipped: 0
        }
    );
    let mut conn = pool.acquire().await.unwrap();
    let latest = statistics::latest(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.data_files, 5);
    assert_eq!(latest.record_count, 5);
    assert_eq!(latest.metadata_location, fixture.metadata_location());
    assert_eq!(latest.catalog_connection_id, tenant.connection);
    assert!(latest.data_bytes > 0);
    assert_eq!(latest.size_histogram.as_array().unwrap().len(), 6);

    // The commit the sample was taken from reaches the snapshot ledger.
    let recorded =
        statistics::snapshots(&mut conn, tenant.organization, tenant.table, None, None, 10)
            .await
            .unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].snapshot_id, 1);
    assert_eq!(recorded[0].operation, "append");

    // The same metadata location is not sampled again, even after the interval.
    let summary = collector.pass_at(now + TimeDelta::hours(1)).await.unwrap();
    assert_eq!(
        summary,
        SampleSummary {
            sampled: 0,
            unchanged: 1,
            skipped: 0
        }
    );
    let history = statistics::history(&mut conn, tenant.organization, tenant.table, None, None, 10)
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    // An unchanged table does not re-record its commits either.
    let repeated =
        statistics::snapshots(&mut conn, tenant.organization, tenant.table, None, None, 10)
            .await
            .unwrap();
    assert_eq!(repeated.len(), 1);
    // The pass that recorded nothing still confirmed the metadata, so the
    // table is fresh even though its sample is an hour old.
    let confirmed = statistics::last_confirmed_at(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap();
    assert_eq!(confirmed, Some(now + TimeDelta::hours(1)));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unchanged_table_replaces_a_sample_without_the_health_counters(pool: PgPool) {
    let fixture = fixture().await;
    let mut server = mockito::Server::new_async().await;
    serve_table(&mut server, &fixture).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, &server.url(), &fixture).await;
    drop(conn);
    let collector = collector(&pool).await;
    let first = chrono::DateTime::from_timestamp_micros(chrono::Utc::now().timestamp_micros())
        .expect("the current instant");

    let summary = collector.pass_at(first).await.unwrap();
    assert_eq!(summary.sampled, 1);

    // A sample written before the health counters existed is immutable: it
    // keeps version 0 and zeros, and retention eventually removes it.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query(
        "UPDATE table_statistics SET sample_version = 0, data_manifest_count = 0, \
         max_partition_files = 0 WHERE organization_id = $1 AND table_identity_id = $2",
    )
    .bind(tenant.organization.as_uuid())
    .bind(tenant.table.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();

    // The metadata is unchanged, but the incomplete sample is replaced rather
    // than taken as unchanged, so the health counters are filled.
    let second = first + TimeDelta::hours(1);
    let summary = collector.pass_at(second).await.unwrap();
    assert_eq!(
        summary,
        SampleSummary {
            sampled: 1,
            unchanged: 0,
            skipped: 0
        }
    );

    let history = statistics::history(&mut conn, tenant.organization, tenant.table, None, None, 10)
        .await
        .unwrap();
    assert_eq!(history.len(), 2, "the legacy sample stays as history");
    assert_eq!(history[1].sample_version, 0);
    assert_eq!(history[1].data_manifest_count, 0);
    assert_eq!(history[1].max_partition_files, 0);
    assert_eq!(history[0].sample_version, statistics::SAMPLE_VERSION);
    assert!(history[0].data_manifest_count >= 1);
    assert!(history[0].max_partition_files >= 1);
    assert_eq!(history[0].metadata_location, history[1].metadata_location);
    assert_eq!(history[0].sampled_at, second);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unchanged_table_backfills_an_empty_snapshot_ledger(pool: PgPool) {
    let fixture = fixture().await;
    let mut server = mockito::Server::new_async().await;
    serve_table(&mut server, &fixture).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, &server.url(), &fixture).await;
    drop(conn);
    let collector = collector(&pool).await;
    // PostgreSQL keeps microseconds, so the instants the test compares must
    // already be truncated or a same-instant assertion can differ by nanos.
    let now = chrono::DateTime::from_timestamp_micros(chrono::Utc::now().timestamp_micros())
        .expect("the current instant");
    collector.pass_at(now).await.unwrap();

    // A table sampled before the ledger existed has a sample but no commits.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("DELETE FROM table_snapshots")
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(
        !statistics::has_snapshot(&mut conn, tenant.organization, tenant.table, 1)
            .await
            .unwrap()
    );
    drop(conn);

    let summary = collector.pass_at(now + TimeDelta::hours(1)).await.unwrap();
    assert_eq!(summary.unchanged, 1);
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        statistics::has_snapshot(&mut conn, tenant.organization, tenant.table, 1)
            .await
            .unwrap()
    );
    let recorded =
        statistics::snapshots(&mut conn, tenant.organization, tenant.table, None, None, 10)
            .await
            .unwrap();
    assert_eq!(recorded.len(), 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unchanged_table_repairs_a_ledger_that_is_behind(pool: PgPool) {
    let fixture = committed_twice().await;
    let mut server = mockito::Server::new_async().await;
    serve_table(&mut server, &fixture).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, &server.url(), &fixture).await;
    drop(conn);
    let collector = collector(&pool).await;
    // PostgreSQL keeps microseconds, so the instants the test compares must
    // already be truncated or a same-instant assertion can differ by nanos.
    let now = chrono::DateTime::from_timestamp_micros(chrono::Utc::now().timestamp_micros())
        .expect("the current instant");
    collector.pass_at(now).await.unwrap();

    // A write that did not commit leaves the newest commit unrecorded while
    // the older one is still there.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("DELETE FROM table_snapshots WHERE snapshot_id = 2")
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(
        !statistics::has_snapshot(&mut conn, tenant.organization, tenant.table, 2)
            .await
            .unwrap()
    );
    assert!(
        statistics::has_snapshot(&mut conn, tenant.organization, tenant.table, 1)
            .await
            .unwrap()
    );
    drop(conn);

    // The unchanged metadata still holds the commit, so the next pass
    // restores it without duplicating the older row.
    let summary = collector.pass_at(now + TimeDelta::hours(1)).await.unwrap();
    assert_eq!(summary.unchanged, 1);
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        statistics::has_snapshot(&mut conn, tenant.organization, tenant.table, 2)
            .await
            .unwrap()
    );
    let recorded =
        statistics::snapshots(&mut conn, tenant.organization, tenant.table, None, None, 10)
            .await
            .unwrap();
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].snapshot_id, 2);
    assert_eq!(recorded[1].snapshot_id, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_table_that_cannot_be_read_does_not_fail_the_pass(pool: PgPool) {
    let fixture = fixture().await;
    // No mocks: every catalog request fails.
    let server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, &server.url(), &fixture).await;
    drop(conn);
    let collector = collector(&pool).await;

    let summary = collector.pass_at(chrono::Utc::now()).await.unwrap();
    assert_eq!(
        summary,
        SampleSummary {
            sampled: 0,
            unchanged: 0,
            skipped: 1
        }
    );
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        statistics::latest(&mut conn, tenant.organization, tenant.table)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_broken_alias_falls_back_to_a_working_one(pool: PgPool) {
    let fixture = fixture().await;
    // The first alias cannot be read; the second one can.
    let dead = mockito::Server::new_async().await;
    let mut healthy = mockito::Server::new_async().await;
    serve_table(&mut healthy, &fixture).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, &dead.url(), &fixture).await;
    // A fixed high identifier sorts after the tenant's random connection.
    let healthy_id = CatalogConnectionId::from_uuid(uuid::Uuid::from_u128(u128::MAX));
    tenancy::create_catalog_connection(
        &mut conn,
        &NewCatalogConnection {
            id: healthy_id,
            organization_id: tenant.organization,
            name: "healthy".to_owned(),
            uri: healthy.url(),
            warehouse: None,
            credential_ref: None,
        },
    )
    .await
    .unwrap();
    tables::resolve_table(
        &mut conn,
        tenant.organization,
        &TableAlias {
            connection_id: healthy_id,
            namespace: vec!["db".to_owned()],
            name: "events".to_owned(),
        },
        fixture.metadata().uuid(),
    )
    .await
    .unwrap();
    drop(conn);
    let collector = collector(&pool).await;

    let summary = collector.pass_at(chrono::Utc::now()).await.unwrap();
    assert_eq!(summary.sampled, 1, "the working alias records the sample");
    let mut conn = pool.acquire().await.unwrap();
    let latest = statistics::latest(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.catalog_connection_id, healthy_id);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_published_table_is_sampled_on_demand(pool: PgPool) {
    let fixture = fixture().await;
    let mut server = mockito::Server::new_async().await;
    serve_table(&mut server, &fixture).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, &server.url(), &fixture).await;
    let job = jobs::create_job(
        &mut conn,
        &NewJob {
            id: JobId::generate(),
            organization_id: tenant.organization,
            table_id: tenant.table,
            operation: JobOperation::Compact,
            retry_of: None,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();
    drop(conn);
    let collector = collector(&pool).await;

    assert_eq!(
        collector.sample_job(&job).await.unwrap(),
        SampleOutcome::Sampled
    );
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        statistics::latest(&mut conn, tenant.organization, tenant.table)
            .await
            .unwrap()
            .is_some()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn storage_collection_rescans_unchanged_tables_and_keeps_a_sample_after_failure(
    pool: PgPool,
) {
    use bergsmith_coordinator::runtime::storage_breakdown::{StorageCollector, StorageSettings};
    use bergsmith_persistence::{leadership, storage_breakdown as ledger};
    let fixture = committed_twice().await;
    let mut server = mockito::Server::new_async().await;
    serve_table(&mut server, &fixture).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, &server.url(), &fixture).await;
    let epoch = leadership::acquire_leadership(
        &mut conn,
        "storage-test",
        std::time::Duration::from_secs(300),
    )
    .await
    .unwrap()
    .unwrap()
    .epoch;
    drop(conn);
    let database = Database::from_pool(pool.clone());
    let catalogs = Arc::new(RuntimeCatalogs::new(
        database,
        RuntimeProfiles {
            default: StorageProfiles::new([StorageProfile::LocalFs]),
            connections: BTreeMap::new(),
        },
    ));
    let settings = StorageSettings {
        enabled: true,
        ..StorageSettings::default()
    };
    let collector = StorageCollector::new(pool.clone(), catalogs, settings, 0, epoch);
    collector.pass().await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let sample = ledger::latest(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sample.content.as_ref().unwrap()["live"]["files"], 2);
    sqlx::query("UPDATE table_storage SET checked_at=checked_at-interval '2 hours'")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    collector.pass().await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let newer = ledger::latest(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap()
        .unwrap();
    assert!(
        newer.sampled_at > sample.sampled_at,
        "unchanged metadata must not skip a due listing"
    );
    sqlx::query("UPDATE table_storage SET checked_at=checked_at-interval '2 hours'")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    server.reset();
    collector.pass().await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let failed = ledger::latest(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.sampled_at, newer.sampled_at);
    assert!(failed.failure_reason.is_some());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn storage_collection_discards_a_changed_baseline_and_disabled_collection_does_no_io(
    pool: PgPool,
) {
    use bergsmith_coordinator::runtime::storage_breakdown::{StorageCollector, StorageSettings};
    use bergsmith_persistence::{leadership, storage_breakdown as ledger};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fixture = fixture().await;
    let mut server = mockito::Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .create_async()
        .await;
    let body = json!({"metadata-location":fixture.metadata_location(),"metadata":serde_json::to_value(fixture.metadata()).unwrap(),"config":{}});
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let table_mock = server
        .mock("GET", "/v1/namespaces/db/tables/events")
        .with_status(200)
        .with_header("content-type", "application/json")
        .expect(2)
        .with_body_from_request(move |_| {
            let mut body = body.clone();
            if observed.fetch_add(1, Ordering::SeqCst) > 0 {
                body["metadata-location"] = json!("file:///changed-metadata.json");
            }
            body.to_string().into_bytes()
        })
        .create_async()
        .await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, &server.url(), &fixture).await;
    let epoch = leadership::acquire_leadership(
        &mut conn,
        "storage-baseline",
        std::time::Duration::from_secs(300),
    )
    .await
    .unwrap()
    .unwrap()
    .epoch;
    drop(conn);
    let database = Database::from_pool(pool.clone());
    let catalogs = Arc::new(RuntimeCatalogs::new(
        database,
        RuntimeProfiles {
            default: StorageProfiles::new([StorageProfile::LocalFs]),
            connections: BTreeMap::new(),
        },
    ));
    let disabled = StorageCollector::new(
        pool.clone(),
        catalogs.clone(),
        StorageSettings::default(),
        0,
        epoch,
    );
    disabled.pass().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let enabled = StorageCollector::new(
        pool.clone(),
        catalogs,
        StorageSettings {
            enabled: true,
            ..StorageSettings::default()
        },
        0,
        epoch,
    );
    enabled.pass().await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let checked = ledger::latest(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap()
        .unwrap();
    assert!(checked.content.is_none());
    assert_eq!(
        checked.failure_reason.as_deref(),
        Some("StorageBaselineChanged")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    config.assert_async().await;
    table_mock.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn storage_recheck_failure_does_not_repeat_the_scan_through_another_alias(pool: PgPool) {
    use bergsmith_coordinator::runtime::storage_breakdown::{StorageCollector, StorageSettings};
    use bergsmith_persistence::{leadership, storage_breakdown as ledger};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fixture = fixture().await;
    let mut server = mockito::Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .create_async()
        .await;
    let body = json!({"metadata-location":fixture.metadata_location(),"metadata":serde_json::to_value(fixture.metadata()).unwrap(),"config":{}}).to_string();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let initial_body = body.clone();
    let primary = server
        .mock("GET", "/v1/namespaces/db/tables/events")
        .with_status(200)
        .with_header("content-type", "application/json")
        .expect(2)
        .with_body_from_request(move |_| {
            if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                initial_body.clone().into_bytes()
            } else {
                b"invalid catalog response during recheck".to_vec()
            }
        })
        .create_async()
        .await;
    let fallback = server
        .mock("GET", "/v1/namespaces/db/tables/z_fallback")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body)
        .expect(0)
        .create_async()
        .await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, &server.url(), &fixture).await;
    tables::resolve_table(
        &mut conn,
        tenant.organization,
        &TableAlias {
            connection_id: tenant.connection,
            namespace: vec!["db".to_owned()],
            name: "z_fallback".to_owned(),
        },
        fixture.metadata().uuid(),
    )
    .await
    .unwrap();
    let epoch = leadership::acquire_leadership(
        &mut conn,
        "storage-recheck",
        std::time::Duration::from_secs(300),
    )
    .await
    .unwrap()
    .unwrap()
    .epoch;
    drop(conn);
    let catalogs = Arc::new(RuntimeCatalogs::new(
        Database::from_pool(pool.clone()),
        RuntimeProfiles {
            default: StorageProfiles::new([StorageProfile::LocalFs]),
            connections: BTreeMap::new(),
        },
    ));
    StorageCollector::new(
        pool.clone(),
        catalogs,
        StorageSettings {
            enabled: true,
            ..StorageSettings::default()
        },
        0,
        epoch,
    )
    .pass()
    .await
    .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let latest = ledger::latest(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        latest.failure_reason.as_deref(),
        Some("StorageBaselineUnavailable")
    );
    assert!(
        latest.content.is_none(),
        "an unverified observation must not be published"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    config.assert_async().await;
    primary.assert_async().await;
    fallback.assert_async().await;
}
