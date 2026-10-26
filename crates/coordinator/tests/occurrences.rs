//! Due-schedule admission through the coordinator runner (BS-019.T2).
#![expect(clippy::unwrap_used, reason = "integration setup and assertions")]

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

use bergsmith_common::health::{FindingKind, HealthThresholds, HealthTrigger, Severity};
use bergsmith_common::ids::{PolicyId, PrincipalId, RoleGrantId, ScheduleId};
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_common::policy::PolicySpec;
use bergsmith_common::schedule::CronSchedule;
use bergsmith_common::window::MaintenanceWindow;
use bergsmith_coordinator::schedules::ScheduleRunner;
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::occurrences;
use bergsmith_persistence::schedules::{
    self, NewMaintenancePolicy, NewMaintenanceSchedule, PolicyUpdate, ScheduleUpdate,
};
use bergsmith_persistence::statistics::{self, NewTableSample};
use bergsmith_persistence::tenancy::{self, NewPrincipal, PrincipalKind};
use chrono::{DateTime, TimeZone, Utc};
use sqlx::{PgConnection, PgPool};

struct Fixture {
    tenant: support::Tenant,
    service: PrincipalId,
    policy: PolicyId,
    schedule: ScheduleId,
    epoch: i64,
}

/// A sample of `files` data files of `bytes_each` bytes, plus its confirmation.
async fn record_sample(
    conn: &mut PgConnection,
    tenant: &support::Tenant,
    at: DateTime<Utc>,
    files: u64,
    bytes_each: u64,
) {
    statistics::record(
        conn,
        tenant.organization,
        tenant.table,
        &NewTableSample {
            catalog_connection_id: tenant.connection,
            sampled_at: at,
            metadata_location: "s3://bucket/table/metadata/v1.metadata.json".to_owned(),
            snapshot_id: Some(1),
            last_operation: Some("append".to_owned()),
            record_count: files,
            data_files: files,
            data_bytes: files * bytes_each,
            delete_files: 0,
            delete_bytes: 0,
            position_delete_files: 0,
            equality_delete_files: 0,
            position_delete_records: 0,
            equality_delete_records: 0,
            manifest_count: 1,
            data_manifest_count: 1,
            manifest_bytes: 1_024,
            partition_count: 0,
            max_partition_files: 0,
            small_files: 0,
            target_file_size_bytes: 512 * 1024 * 1024,
            snapshot_count: 1,
            oldest_snapshot_at: Some(at),
            newest_snapshot_at: Some(at),
            size_histogram: serde_json::json!([]),
            top_partitions: serde_json::json!([]),
        },
    )
    .await
    .unwrap();
    statistics::record_check(conn, tenant.organization, tenant.table, at)
        .await
        .unwrap();
}

async fn arm_trigger(conn: &mut PgConnection, fixture: &Fixture, trigger: HealthTrigger) {
    let policy = schedules::policy(conn, fixture.tenant.organization, fixture.policy)
        .await
        .unwrap();
    schedules::update_policy_with_health(
        conn,
        fixture.tenant.organization,
        fixture.policy,
        &PolicyUpdate {
            name: None,
            specification: None,
            enabled: None,
            expected_revision: policy.revision,
        },
        Some(HealthThresholds::default()),
        Some(Some(trigger)),
    )
    .await
    .unwrap();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_runner_admits_only_when_the_policy_trigger_is_met(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme").await;
    let now = at(10);
    set_due(&mut conn, &fixture, now).await;
    arm_trigger(
        &mut conn,
        &fixture,
        HealthTrigger::new(FindingKind::SmallFiles, Severity::Warning),
    )
    .await;
    // Four MiB per file: the sample reports critical small files.
    record_sample(&mut conn, &fixture.tenant, now, 5, 4 * 1024 * 1024).await;
    drop(conn);

    let runner = ScheduleRunner::new(pool.clone(), fixture.epoch, 16);
    let summary = runner.admit_due_at(now).await.unwrap();
    assert_eq!(summary.admitted, 1);
    assert_eq!(summary.skipped, 0);

    let mut conn = pool.acquire().await.unwrap();
    let (kind, severity): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT trigger_finding, trigger_severity FROM jobs
         WHERE organization_id = $1 AND table_identity_id = $2",
    )
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.tenant.table.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(kind.as_deref(), Some("small_files"));
    assert_eq!(severity.as_deref(), Some("warning"));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_runner_skips_when_the_sample_is_healthy(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme").await;
    let now = at(10);
    set_due(&mut conn, &fixture, now).await;
    arm_trigger(
        &mut conn,
        &fixture,
        HealthTrigger::new(FindingKind::SmallFiles, Severity::Warning),
    )
    .await;
    // Half a gigabyte per file: no finding fires.
    record_sample(&mut conn, &fixture.tenant, now, 2, 512 * 1024 * 1024).await;
    drop(conn);

    let runner = ScheduleRunner::new(pool.clone(), fixture.epoch, 16);
    let summary = runner.admit_due_at(now).await.unwrap();
    assert_eq!(summary.admitted, 0);
    assert_eq!(summary.skipped, 1);

    let mut conn = pool.acquire().await.unwrap();
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE organization_id = $1 AND table_identity_id = $2",
    )
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.tenant.table.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(jobs, 0);
    let reason: Option<String> = sqlx::query_scalar(
        "SELECT reason FROM maintenance_schedule_occurrences
         WHERE organization_id = $1 AND schedule_id = $2
         ORDER BY scheduled_for DESC LIMIT 1",
    )
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.schedule.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(reason.as_deref(), Some("HealthTriggerNotMet"));
}

async fn fixture(conn: &mut PgConnection, slug: &str) -> Fixture {
    let tenant = support::tenant(conn, slug).await;
    let service = PrincipalId::generate();
    tenancy::create_principal(
        conn,
        &NewPrincipal {
            id: service,
            organization_id: tenant.organization,
            kind: PrincipalKind::Service,
            issuer: "bergsmith:test".into(),
            subject: "scheduler".into(),
            display_name: "scheduler".into(),
        },
    )
    .await
    .unwrap();
    grants::create_grant(
        conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: service,
            role: Role::MaintenanceService,
            scope: GrantScope::table(tenant.table),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    let policy = PolicyId::generate();
    schedules::create_policy(
        conn,
        &NewMaintenancePolicy {
            id: policy,
            organization_id: tenant.organization,
            name: "nightly".to_owned(),
            specification: PolicySpec::default(),
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();
    let schedule = ScheduleId::generate();
    schedules::create_schedule(
        conn,
        &NewMaintenanceSchedule {
            id: schedule,
            organization_id: tenant.organization,
            policy_id: policy,
            table_id: tenant.table,
            timing: CronSchedule::parse("0 * * * *", "UTC").unwrap(),
            window: None,
            service_principal_id: service,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();
    let epoch = support::leader(conn).await;
    Fixture {
        tenant,
        service,
        policy,
        schedule,
        epoch,
    }
}

/// Creates a second policy and schedule for the same table and service.
async fn second_schedule(conn: &mut PgConnection, fixture: &Fixture) -> ScheduleId {
    let policy = PolicyId::generate();
    schedules::create_policy(
        conn,
        &NewMaintenancePolicy {
            id: policy,
            organization_id: fixture.tenant.organization,
            name: "hourly".to_owned(),
            specification: PolicySpec::default(),
            created_by: fixture.tenant.user,
        },
    )
    .await
    .unwrap();
    let schedule = ScheduleId::generate();
    schedules::create_schedule(
        conn,
        &NewMaintenanceSchedule {
            id: schedule,
            organization_id: fixture.tenant.organization,
            policy_id: policy,
            table_id: fixture.tenant.table,
            timing: CronSchedule::parse("0 * * * *", "UTC").unwrap(),
            window: None,
            service_principal_id: fixture.service,
            created_by: fixture.tenant.user,
        },
    )
    .await
    .unwrap();
    schedule
}

async fn set_due(conn: &mut PgConnection, fixture: &Fixture, due: DateTime<Utc>) {
    set_schedule_due(conn, fixture.tenant.organization, fixture.schedule, due).await;
}

async fn set_schedule_due(
    conn: &mut PgConnection,
    organization: bergsmith_common::ids::OrganizationId,
    schedule: ScheduleId,
    due: DateTime<Utc>,
) {
    sqlx::query(
        "UPDATE maintenance_schedule_state SET next_due_at = $3
         WHERE organization_id = $1 AND schedule_id = $2",
    )
    .bind(organization.as_uuid())
    .bind(schedule.as_uuid())
    .bind(due)
    .execute(conn)
    .await
    .unwrap();
}

fn at(hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 1, hour, 0, 0)
        .single()
        .unwrap()
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_runner_admits_one_job_and_reports_an_empty_next_pass(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme").await;
    set_due(&mut conn, &fixture, at(10)).await;
    drop(conn);

    let runner = ScheduleRunner::new(pool.clone(), fixture.epoch, 16);
    let first = runner.admit_due_at(at(10)).await.unwrap();
    assert_eq!(first.admitted, 1);
    assert_eq!(first.coalesced, 0);
    assert_eq!(first.skipped, 0);

    let second = runner.admit_due_at(at(10)).await.unwrap();
    assert_eq!(second.admitted, 0);

    let mut conn = pool.acquire().await.unwrap();
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE organization_id=$1 AND table_identity_id=$2",
    )
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.tenant.table.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(jobs, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_runner_reports_a_disabled_policy_as_skipped(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme").await;
    schedules::update_policy(
        &mut conn,
        fixture.tenant.organization,
        fixture.policy,
        &PolicyUpdate {
            name: None,
            specification: None,
            enabled: Some(false),
            expected_revision: 1,
        },
    )
    .await
    .unwrap();
    set_due(&mut conn, &fixture, at(10)).await;
    drop(conn);

    let runner = ScheduleRunner::new(pool.clone(), fixture.epoch, 16);
    let summary = runner.admit_due_at(at(10)).await.unwrap();
    assert_eq!(summary.admitted, 0);
    assert_eq!(summary.skipped, 1);

    let mut conn = pool.acquire().await.unwrap();
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE organization_id=$1 AND table_identity_id=$2",
    )
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.tenant.table.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(jobs, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_runner_arms_schedules_that_predate_the_state_table(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme").await;
    sqlx::query("DELETE FROM maintenance_schedule_state WHERE schedule_id=$1")
        .bind(fixture.schedule.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);

    let runner = ScheduleRunner::new(pool.clone(), fixture.epoch, 16);
    let summary = runner.admit_due_at(at(10)).await.unwrap();
    assert_eq!(summary.admitted, 0);

    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(11))
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_failing_schedule_does_not_starve_later_schedules(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme").await;
    let healthy = second_schedule(&mut conn, &fixture).await;
    sqlx::query("UPDATE maintenance_schedules SET cron_expression='not a cron' WHERE id=$1")
        .bind(fixture.schedule.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    set_due(&mut conn, &fixture, at(10)).await;
    set_schedule_due(&mut conn, fixture.tenant.organization, healthy, at(10)).await;
    drop(conn);

    // One slot per pass: the broken schedule is attempted first, because its
    // identifier is lower, and fails. The next pass must move past it instead
    // of retrying it forever.
    let runner = ScheduleRunner::new(pool.clone(), fixture.epoch, 1);
    let first = runner.admit_due_at(at(10)).await.unwrap();
    assert_eq!(first.admitted, 0);
    let second = runner.admit_due_at(at(10)).await.unwrap();
    assert_eq!(second.admitted, 1);

    let mut conn = pool.acquire().await.unwrap();
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE organization_id=$1 AND table_identity_id=$2",
    )
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.tenant.table.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(jobs, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_runner_reports_an_occurrence_outside_its_window_as_skipped(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme").await;
    schedules::update_schedule(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: Some(Some(MaintenanceWindow::new(120, 240).unwrap())),
            enabled: None,
            expected_revision: 1,
        },
    )
    .await
    .unwrap();
    // Due at 10:00 UTC, which is outside 02:00–04:00.
    set_due(&mut conn, &fixture, at(10)).await;
    drop(conn);

    let runner = ScheduleRunner::new(pool.clone(), fixture.epoch, 16);
    let summary = runner.admit_due_at(at(10)).await.unwrap();
    assert_eq!(summary.admitted, 0);
    assert_eq!(summary.coalesced, 0);
    assert_eq!(summary.skipped, 1);

    let mut conn = pool.acquire().await.unwrap();
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE organization_id=$1 AND table_identity_id=$2",
    )
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.tenant.table.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(jobs, 0);
    let reason: Option<String> = sqlx::query_scalar(
        "SELECT reason FROM maintenance_schedule_occurrences WHERE schedule_id=$1",
    )
    .bind(fixture.schedule.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(reason.as_deref(), Some("OutsideWindow"));
    // The cursor waits for tomorrow's opening, not the next hourly tick.
    let next: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT next_due_at FROM maintenance_schedule_state WHERE schedule_id=$1",
    )
    .bind(fixture.schedule.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        next,
        Some(Utc.with_ymd_and_hms(2026, 5, 2, 2, 0, 0).single().unwrap())
    );
    // A routine outside-window evaluation is not audited as a denial.
    let denied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE organization_id=$1 AND action='job.admission_denied' AND resource_id=$2",
    )
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.schedule.to_string())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(denied, 0);
}
