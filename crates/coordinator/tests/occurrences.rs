//! Due-schedule admission through the coordinator runner (BS-019.T2).
#![expect(clippy::unwrap_used, reason = "integration setup and assertions")]

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

use bergsmith_common::ids::{PolicyId, PrincipalId, RoleGrantId, ScheduleId};
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_common::policy::PolicySpec;
use bergsmith_common::schedule::CronSchedule;
use bergsmith_coordinator::schedules::ScheduleRunner;
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::occurrences;
use bergsmith_persistence::schedules::{
    self, NewMaintenancePolicy, NewMaintenanceSchedule, PolicyUpdate,
};
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
