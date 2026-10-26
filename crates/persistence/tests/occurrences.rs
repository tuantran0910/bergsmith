//! Schedule occurrence admission against real PostgreSQL (BS-019.T2).
#![expect(clippy::unwrap_used, reason = "integration setup and assertions")]

mod support;

use bergsmith_common::health::{
    FindingKind, HealthFinding, HealthThresholds, HealthTrigger, HealthVerdict, Severity,
};
use bergsmith_common::ids::{OrganizationId, PolicyId, PrincipalId, RoleGrantId, ScheduleId};
use bergsmith_common::job::JobState;
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_common::policy::PolicySpec;
use bergsmith_common::schedule::CronSchedule;
use bergsmith_common::window::MaintenanceWindow;
use bergsmith_persistence::cancellation::{self, SendAuthorization};
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::occurrences::{self, OccurrenceState};
use bergsmith_persistence::publications;
use bergsmith_persistence::schedules::{
    self, NewMaintenancePolicy, NewMaintenanceSchedule, PolicyUpdate, ScheduleUpdate,
};
use bergsmith_persistence::tenancy::{self, NewPrincipal, PrincipalKind};
use bergsmith_persistence::{jobs, schedules::MaintenanceSchedule};
use chrono::{DateTime, TimeZone, Utc};
use sqlx::{PgConnection, PgPool};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use uuid::Uuid;

struct Fixture {
    tenant: support::Tenant,
    service: PrincipalId,
    grant: RoleGrantId,
    policy: PolicyId,
    schedule: ScheduleId,
    epoch: i64,
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_paused_connection_skips_due_occurrences_and_resumes_after_resuming(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;
    sqlx::query("UPDATE catalog_connections SET maintenance_enabled=false WHERE id=$1")
        .bind(fixture.tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(occurrence.state, OccurrenceState::Skipped);
    assert_eq!(occurrence.reason.as_deref(), Some("MaintenancePaused"));
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 0);
    sqlx::query("UPDATE catalog_connections SET maintenance_enabled=true WHERE id=$1")
        .bind(fixture.tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let occurrence = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 5, 1, 11, 0),
        fixture.epoch,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(occurrence.state, OccurrenceState::Admitted);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn disabled_connections_skip_due_occurrences_and_resume_after_enable(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;
    sqlx::query("UPDATE catalog_connections SET enabled=false WHERE id=$1")
        .bind(fixture.tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(occurrence.state, OccurrenceState::Skipped);
    assert_eq!(occurrence.reason.as_deref(), Some("ConnectionDisabled"));
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 0);
    sqlx::query("UPDATE catalog_connections SET enabled=true WHERE id=$1")
        .bind(fixture.tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let occurrence = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 5, 1, 11, 0),
        fixture.epoch,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(occurrence.state, OccurrenceState::Admitted);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_met_health_trigger_admits_and_the_job_names_its_cause(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let trigger = HealthTrigger::new(FindingKind::SmallFiles, Severity::Warning);
    arm_trigger(&mut conn, &fixture, trigger, HealthThresholds::default()).await;
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;
    let verdict = HealthVerdict::new(vec![small_files(Severity::Critical)]);

    let occurrence = occurrences::admit_with_health(
        &mut conn,
        fixture.schedule,
        fixture.epoch,
        || now,
        Some(&verdict),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(occurrence.state, OccurrenceState::Admitted);
    let job = jobs::job_by_id(&mut conn, occurrence.job_id.expect("an admitted job"))
        .await
        .unwrap();
    assert_eq!(job.trigger, Some(trigger));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unmet_health_trigger_skips_without_creating_work(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let trigger = HealthTrigger::new(FindingKind::SmallFiles, Severity::Critical);
    arm_trigger(&mut conn, &fixture, trigger, HealthThresholds::default()).await;
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;
    // A warning does not reach a critical condition.
    let verdict = HealthVerdict::new(vec![small_files(Severity::Warning)]);

    let occurrence = occurrences::admit_with_health(
        &mut conn,
        fixture.schedule,
        fixture.epoch,
        || now,
        Some(&verdict),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(occurrence.state, OccurrenceState::Skipped);
    assert_eq!(occurrence.reason.as_deref(), Some("HealthTriggerNotMet"));
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 0);

    // An unassessed table (no verdict) does not meet the trigger either.
    let next = at(2026, 5, 1, 11, 0);
    set_due(&mut conn, &fixture, next).await;
    let occurrence =
        occurrences::admit_with_health(&mut conn, fixture.schedule, fixture.epoch, || next, None)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(occurrence.state, OccurrenceState::Skipped);
    assert_eq!(occurrence.reason.as_deref(), Some("HealthTriggerNotMet"));
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_triggered_evaluation_still_coalesces_into_an_active_job(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let trigger = HealthTrigger::new(FindingKind::SmallFiles, Severity::Warning);
    arm_trigger(&mut conn, &fixture, trigger, HealthThresholds::default()).await;
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;
    let verdict = HealthVerdict::new(vec![small_files(Severity::Critical)]);

    let first = occurrences::admit_with_health(
        &mut conn,
        fixture.schedule,
        fixture.epoch,
        || now,
        Some(&verdict),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.state, OccurrenceState::Admitted);

    let next = at(2026, 5, 1, 11, 0);
    set_due(&mut conn, &fixture, next).await;
    let second = occurrences::admit_with_health(
        &mut conn,
        fixture.schedule,
        fixture.epoch,
        || next,
        Some(&verdict),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(second.state, OccurrenceState::Coalesced);
    assert_eq!(second.job_id, first.job_id);
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 1);
}

/// Sets the fixture policy's trigger and thresholds.
async fn arm_trigger(
    conn: &mut PgConnection,
    fixture: &Fixture,
    trigger: HealthTrigger,
    thresholds: HealthThresholds,
) {
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
        Some(thresholds),
        Some(Some(trigger)),
    )
    .await
    .unwrap();
}

fn small_files(severity: Severity) -> HealthFinding {
    HealthFinding {
        kind: FindingKind::SmallFiles,
        severity,
    }
}

async fn fixture(conn: &mut PgConnection, slug: &str, cron: &str, zone: &str) -> Fixture {
    let tenant = support::tenant(conn, slug).await;
    let service = service_principal(conn, tenant.organization, "scheduler").await;
    let grant = grants::create_grant(
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
    .unwrap()
    .id;
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
            timing: CronSchedule::parse(cron, zone).unwrap(),
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
        grant,
        policy,
        schedule,
        epoch,
    }
}

async fn service_principal(
    conn: &mut PgConnection,
    organization: OrganizationId,
    subject: &str,
) -> PrincipalId {
    let id = PrincipalId::generate();
    tenancy::create_principal(
        conn,
        &NewPrincipal {
            id,
            organization_id: organization,
            kind: PrincipalKind::Service,
            issuer: "bergsmith:test".into(),
            subject: subject.to_owned(),
            display_name: subject.to_owned(),
        },
    )
    .await
    .unwrap();
    id
}

async fn set_due(conn: &mut PgConnection, fixture: &Fixture, due: DateTime<Utc>) {
    sqlx::query(
        "UPDATE maintenance_schedule_state SET next_due_at = $3
         WHERE organization_id = $1 AND schedule_id = $2",
    )
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.schedule.as_uuid())
    .bind(due)
    .execute(conn)
    .await
    .unwrap();
}

async fn jobs_on_table(conn: &mut PgConnection, tenant: &support::Tenant) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE organization_id=$1 AND table_identity_id=$2",
    )
    .bind(tenant.organization.as_uuid())
    .bind(tenant.table.as_uuid())
    .fetch_one(conn)
    .await
    .unwrap()
}

fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .unwrap()
}

/// Waits until PostgreSQL reports `application_name` waiting on a lock.
///
/// Synchronizing on the server, rather than a sleep, proves that the session is
/// actually blocked before a test inspects what it has done so far.
async fn wait_until_blocked(pool: &PgPool, application_name: &str) {
    let mut blocked = false;
    for _ in 0..200 {
        blocked = sqlx::query_scalar(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_stat_activity
                 WHERE application_name = $1 AND wait_event_type = 'Lock'
             )",
        )
        .bind(application_name)
        .fetch_one(pool)
        .await
        .unwrap();
        if blocked {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        blocked,
        "connection {application_name} never reported blocking on a lock"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_due_occurrence_admits_one_job_as_the_service_identity(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;

    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .expect("an occurrence is due");
    assert_eq!(occurrence.state, OccurrenceState::Admitted);
    assert_eq!(occurrence.scheduled_for, now);
    assert_eq!(occurrence.coalesced_until, None);
    let job_id = occurrence.job_id.unwrap();
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 1);

    let job = jobs::job(&mut conn, fixture.tenant.organization, job_id)
        .await
        .unwrap();
    assert_eq!(job.created_by, fixture.service);
    assert_eq!(job.table_id, fixture.tenant.table);
    assert_eq!(job.state, JobState::Planning);
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 1, 11, 0))
    );

    let audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE organization_id=$1 AND actor_principal_id=$2
           AND action='job.submitted' AND resource_id=$3",
    )
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.service.as_uuid())
    .bind(job_id.to_string())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(audit, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn replaying_the_same_instant_admits_only_one_job(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;

    assert!(
        occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 1);
    assert_eq!(
        occurrences::occurrences(&mut conn, fixture.tenant.organization, fixture.schedule, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn downtime_coalesces_missed_occurrences_into_one_evaluation(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    set_due(&mut conn, &fixture, at(2026, 5, 1, 10, 0)).await;

    let now = at(2026, 5, 1, 13, 30);
    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .expect("the missed occurrences are due");
    assert_eq!(occurrence.state, OccurrenceState::Admitted);
    assert_eq!(occurrence.scheduled_for, at(2026, 5, 1, 10, 0));
    assert_eq!(occurrence.coalesced_until, Some(at(2026, 5, 1, 13, 0)));
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 1);
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 1, 14, 0))
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_active_job_on_the_table_coalesces_the_occurrence(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let blocking = support::job(&mut conn, &fixture.tenant).await;
    support::force_job_state(&mut conn, blocking, JobState::Running).await;
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;

    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .expect("the occurrence is due");
    assert_eq!(occurrence.state, OccurrenceState::Coalesced);
    assert_eq!(occurrence.job_id, Some(blocking));
    assert_eq!(occurrence.reason.as_deref(), Some("TableBusy"));
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 1);
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 1, 11, 0))
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_disabled_policy_skips_admission_without_creating_a_job(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
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
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;

    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .expect("the occurrence is due");
    assert_eq!(occurrence.state, OccurrenceState::Skipped);
    assert_eq!(occurrence.reason.as_deref(), Some("PolicyDisabled"));
    assert_eq!(occurrence.job_id, None);
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 0);
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 1, 11, 0))
    );
    let audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE organization_id=$1 AND action='job.admission_denied' AND resource_id=$2",
    )
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.schedule.to_string())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(audit, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_revoked_service_grant_skips_admission(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    grants::revoke_grant(&mut conn, fixture.tenant.organization, fixture.grant, None)
        .await
        .unwrap();
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;

    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .expect("the occurrence is due");
    assert_eq!(occurrence.state, OccurrenceState::Skipped);
    assert_eq!(
        occurrence.reason.as_deref(),
        Some("ServiceIdentityIneligible")
    );
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_disabled_schedule_stops_admission_and_reenabling_does_not_backfill(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let disabled: MaintenanceSchedule = schedules::update_schedule(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: None,
            enabled: Some(false),
            expected_revision: 1,
        },
    )
    .await
    .unwrap();
    assert!(!disabled.enabled);
    assert!(
        occurrences::due(&mut conn, Utc::now(), 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        occurrences::admit_at(
            &mut conn,
            fixture.schedule,
            Utc::now() + chrono::Duration::days(1),
            fixture.epoch,
        )
        .await
        .unwrap()
        .is_none()
    );

    let reenabled = schedules::update_schedule(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: None,
            enabled: Some(true),
            expected_revision: 2,
        },
    )
    .await
    .unwrap();
    assert!(reenabled.enabled);
    let due = occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
        .await
        .unwrap()
        .expect("re-enabling arms the next occurrence");
    assert!(due > Utc::now());

    // Ten simulated days later the schedule is due exactly once: the disabled
    // interval was never materialized as a backlog of occurrences.
    let later = Utc::now() + chrono::Duration::days(10);
    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, later, fixture.epoch)
        .await
        .unwrap()
        .expect("the re-armed schedule is due");
    assert_eq!(occurrence.state, OccurrenceState::Admitted);
    assert!(occurrence.coalesced_until.is_some());
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn spring_forward_gaps_and_fall_back_overlaps_advance_the_cursor(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "30 2 * * *", "Europe/Berlin").await;

    // 2026-03-29 02:30 does not exist in Berlin; the next occurrence is the
    // 30th at 02:30 CEST (00:30 UTC).
    set_due(&mut conn, &fixture, at(2026, 3, 28, 1, 30)).await;
    occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 3, 28, 1, 31),
        fixture.epoch,
    )
    .await
    .unwrap();
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 3, 30, 0, 30))
    );

    // The table now has a Planning job, so the next evaluation coalesces. The
    // cursor still advances: the repeated 02:30 local time runs once, at the
    // earlier instant (00:30 UTC), and the next is the following day.
    set_due(&mut conn, &fixture, at(2026, 10, 25, 0, 30)).await;
    occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 10, 25, 0, 31),
        fixture.epoch,
    )
    .await
    .unwrap();
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 10, 26, 1, 30))
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_disabled_policy_blocks_send_authorization_but_a_disabled_schedule_does_not(
    pool: PgPool,
) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;
    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .unwrap();
    let job = occurrence.job_id.unwrap();
    support::force_job_state(&mut conn, job, JobState::Committing).await;
    let revision = jobs::job(&mut conn, fixture.tenant.organization, job)
        .await
        .unwrap()
        .state_revision;

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
    let denied = cancellation::authorize_send(
        &mut conn,
        SendAuthorization {
            job_id: job,
            expected_revision: revision,
            coordinator_epoch: fixture.epoch,
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            denied,
            bergsmith_persistence::PersistenceError::MaintenancePolicyDisabled { .. }
        ),
        "unexpected error: {denied:?}"
    );

    schedules::update_policy(
        &mut conn,
        fixture.tenant.organization,
        fixture.policy,
        &PolicyUpdate {
            name: None,
            specification: None,
            enabled: Some(true),
            expected_revision: 2,
        },
    )
    .await
    .unwrap();
    schedules::update_schedule(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: None,
            enabled: Some(false),
            expected_revision: 1,
        },
    )
    .await
    .unwrap();
    let authorized = cancellation::authorize_send(
        &mut conn,
        SendAuthorization {
            job_id: job,
            expected_revision: revision,
            coordinator_epoch: fixture.epoch,
        },
    )
    .await
    .unwrap();
    assert!(authorized.publication_started_at.is_some());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_schema_rejects_an_inconsistent_occurrence(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;
    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .unwrap();

    let duplicate = sqlx::query(
        "INSERT INTO maintenance_schedule_occurrences
             (id, organization_id, schedule_id, scheduled_for, state)
         VALUES ($1, $2, $3, $4, 'Pending')",
    )
    .bind(Uuid::now_v7())
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.schedule.as_uuid())
    .bind(occurrence.scheduled_for)
    .execute(&mut *conn)
    .await
    .unwrap_err();
    assert_eq!(support::sqlstate(&duplicate), "23505");

    let missing_job =
        sqlx::query("UPDATE maintenance_schedule_occurrences SET state='Skipped' WHERE id=$1")
            .bind(occurrence.id)
            .execute(&mut *conn)
            .await
            .unwrap_err();
    assert_eq!(support::sqlstate(&missing_job), "23514");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_redundant_enable_preserves_an_overdue_cursor(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let overdue = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, overdue).await;

    // `enabled: true` on an already enabled schedule is not a state transition
    // and must not move an overdue cursor into the future.
    let updated = schedules::update_schedule(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: None,
            enabled: Some(true),
            expected_revision: 1,
        },
    )
    .await
    .unwrap();
    assert!(updated.enabled);
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(overdue)
    );

    // An actual disable/enable cycle re-arms from the current instant.
    schedules::update_schedule(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: None,
            enabled: Some(false),
            expected_revision: 2,
        },
    )
    .await
    .unwrap();
    assert!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap()
            .is_none()
    );
    schedules::update_schedule(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: None,
            enabled: Some(true),
            expected_revision: 3,
        },
    )
    .await
    .unwrap();
    let rearmed = occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
        .await
        .unwrap()
        .expect("re-enabling arms the next occurrence");
    assert!(rearmed > Utc::now());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn arming_missing_state_recovers_schedules_that_predate_t2(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    sqlx::query("DELETE FROM maintenance_schedule_state WHERE schedule_id=$1")
        .bind(fixture.schedule.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap()
            .is_none()
    );

    let now = at(2026, 5, 1, 10, 30);
    assert_eq!(
        occurrences::arm_missing(&mut conn, now, 10).await.unwrap(),
        1
    );
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 1, 11, 0))
    );
    // The row is no longer missing, so later passes do not re-arm it.
    assert_eq!(
        occurrences::arm_missing(&mut conn, now, 10).await.unwrap(),
        0
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn arming_stores_unparseable_timing_dormant_instead_of_rescanning_it(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    sqlx::query("UPDATE maintenance_schedules SET cron_expression='not a cron' WHERE id=$1")
        .bind(fixture.schedule.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("DELETE FROM maintenance_schedule_state WHERE schedule_id=$1")
        .bind(fixture.schedule.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();

    let now = at(2026, 5, 1, 10, 30);
    assert_eq!(
        occurrences::arm_missing(&mut conn, now, 10).await.unwrap(),
        1
    );
    assert!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        occurrences::arm_missing(&mut conn, now, 10).await.unwrap(),
        0
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_policy_denial_resolves_a_ready_to_commit_job_before_an_intent(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;
    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .unwrap();
    let job = occurrence.job_id.unwrap();
    support::force_job_state(&mut conn, job, JobState::ReadyToCommit).await;
    let revision = jobs::job(&mut conn, fixture.tenant.organization, job)
        .await
        .unwrap()
        .state_revision;
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

    // A stale revision is not resolved.
    assert!(
        publications::resolve_pre_intent_denial(
            &mut conn,
            job,
            revision + 1,
            fixture.epoch,
            "PolicyDisabled",
        )
        .await
        .unwrap()
        .is_none()
    );
    let failed = publications::resolve_pre_intent_denial(
        &mut conn,
        job,
        revision,
        fixture.epoch,
        "PolicyDisabled",
    )
    .await
    .unwrap()
    .expect("a ready-to-commit job without an intent is resolved");
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.state_reason.as_deref(), Some("PolicyDisabled"));
    assert!(
        publications::resolve_pre_intent_denial(
            &mut conn,
            job,
            revision,
            fixture.epoch,
            "PolicyDisabled",
        )
        .await
        .unwrap()
        .is_none()
    );

    // The table is free for a later occurrence instead of staying coalesced
    // behind a job that could never publish.
    let next = at(2026, 5, 1, 11, 0);
    set_due(&mut conn, &fixture, next).await;
    schedules::update_policy(
        &mut conn,
        fixture.tenant.organization,
        fixture.policy,
        &PolicyUpdate {
            name: None,
            specification: None,
            enabled: Some(true),
            expected_revision: 2,
        },
    )
    .await
    .unwrap();
    let fresh = occurrences::admit_at(&mut conn, fixture.schedule, next, fixture.epoch)
        .await
        .unwrap()
        .expect("the next occurrence is due");
    assert_eq!(fresh.state, OccurrenceState::Admitted);
    assert_ne!(fresh.job_id, Some(job));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_pre_intent_resolution_refuses_a_job_that_already_has_an_intent(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;
    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .unwrap();
    let job = occurrence.job_id.unwrap();
    support::force_job_state(&mut conn, job, JobState::ReadyToCommit).await;
    let revision = jobs::job(&mut conn, fixture.tenant.organization, job)
        .await
        .unwrap()
        .state_revision;

    let publication = Uuid::now_v7();
    sqlx::query("INSERT INTO publications (id, job_id) VALUES ($1, $2)")
        .bind(publication)
        .bind(job.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO publication_intents
             (job_id, publication_id, content, request_body, request_hash)
         VALUES ($1, $2, '{}'::jsonb, ''::bytea, $3)",
    )
    .bind(job.as_uuid())
    .bind(publication)
    .bind("a".repeat(64))
    .execute(&mut *conn)
    .await
    .unwrap();

    assert!(
        publications::resolve_pre_intent_denial(
            &mut conn,
            job,
            revision,
            fixture.epoch,
            "PolicyDisabled",
        )
        .await
        .unwrap()
        .is_none()
    );
}

/// Replaces the fixture schedule's window, keeping its timing unchanged.
async fn set_window(
    conn: &mut PgConnection,
    fixture: &Fixture,
    window: MaintenanceWindow,
) -> MaintenanceSchedule {
    schedules::update_schedule(
        conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: Some(Some(window)),
            enabled: None,
            expected_revision: 1,
        },
    )
    .await
    .unwrap()
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_occurrence_outside_its_window_is_skipped_without_a_job(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let updated = set_window(
        &mut conn,
        &fixture,
        MaintenanceWindow::new(120, 240).unwrap(),
    )
    .await;
    assert_eq!(
        updated.window,
        Some(MaintenanceWindow::new(120, 240).unwrap())
    );
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;

    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .expect("the occurrence is due");
    assert_eq!(occurrence.state, OccurrenceState::Skipped);
    assert_eq!(occurrence.reason.as_deref(), Some("OutsideWindow"));
    assert_eq!(occurrence.job_id, None);
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 0);
    // The cursor jumps to the next opening's first occurrence instead of
    // returning at the next hourly tick.
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 2, 2, 0))
    );
    // A routine outside-window tick is not an operator denial.
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

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_outside_window_span_is_one_skipped_row(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    set_window(
        &mut conn,
        &fixture,
        MaintenanceWindow::new(120, 240).unwrap(),
    )
    .await;
    // The cursor fell due at 05:00 and recovery happens at 10:00, outside the
    // 02:00–04:00 window. One row covers the whole outside span.
    set_due(&mut conn, &fixture, at(2026, 5, 1, 5, 0)).await;
    let now = at(2026, 5, 1, 10, 0);
    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .expect("the occurrence is due");
    assert_eq!(occurrence.state, OccurrenceState::Skipped);
    assert_eq!(occurrence.scheduled_for, at(2026, 5, 1, 5, 0));
    assert_eq!(occurrence.coalesced_until, Some(now));

    let rows =
        occurrences::occurrences(&mut conn, fixture.tenant.organization, fixture.schedule, 10)
            .await
            .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reason.as_deref(), Some("OutsideWindow"));
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 2, 2, 0))
    );
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_exclusive_window_end_is_outside(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "* * * * *", "UTC").await;
    set_window(
        &mut conn,
        &fixture,
        MaintenanceWindow::new(120, 240).unwrap(),
    )
    .await;
    // 04:00 is the exclusive end of the 02:00–04:00 window.
    let now = at(2026, 5, 1, 4, 0);
    set_due(&mut conn, &fixture, now).await;

    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .expect("the occurrence is due");
    assert_eq!(occurrence.state, OccurrenceState::Skipped);
    assert_eq!(occurrence.reason.as_deref(), Some("OutsideWindow"));
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_missed_occurrence_is_admitted_while_its_window_is_still_open(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    set_window(
        &mut conn,
        &fixture,
        MaintenanceWindow::new(120, 240).unwrap(),
    )
    .await;
    // The occurrence was due at 02:00, but admission happens at 02:30, still
    // inside the window. Lateness alone does not skip it.
    set_due(&mut conn, &fixture, at(2026, 5, 1, 2, 0)).await;
    let occurrence = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 5, 1, 2, 30),
        fixture.epoch,
    )
    .await
    .unwrap()
    .expect("the occurrence is due");
    assert_eq!(occurrence.state, OccurrenceState::Admitted);
    assert_eq!(occurrence.coalesced_until, None);
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn recovery_after_the_window_closed_skips_the_run(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    set_window(
        &mut conn,
        &fixture,
        MaintenanceWindow::new(120, 240).unwrap(),
    )
    .await;
    // The coordinator was down for the 02:00 occurrence and recovered at 05:00,
    // after the window closed. It does not start maintenance during active
    // writes; the next recurrence can admit again.
    set_due(&mut conn, &fixture, at(2026, 5, 1, 2, 0)).await;
    let occurrence = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 5, 1, 5, 0),
        fixture.epoch,
    )
    .await
    .unwrap()
    .expect("the occurrence is due");
    assert_eq!(occurrence.state, OccurrenceState::Skipped);
    assert_eq!(occurrence.reason.as_deref(), Some("OutsideWindow"));
    assert_eq!(occurrence.coalesced_until, Some(at(2026, 5, 1, 5, 0)));
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_window_is_evaluated_in_the_schedule_zone(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "Asia/Tokyo").await;
    // 00:00–01:00 in Tokyo.
    set_window(&mut conn, &fixture, MaintenanceWindow::new(0, 60).unwrap()).await;

    // 15:30 UTC is 00:30 in Tokyo, inside the window.
    set_due(&mut conn, &fixture, at(2026, 5, 1, 15, 30)).await;
    let inside = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 5, 1, 15, 30),
        fixture.epoch,
    )
    .await
    .unwrap()
    .expect("the occurrence is due");
    assert_eq!(inside.state, OccurrenceState::Admitted);

    // 16:30 UTC is 01:30 in Tokyo, outside it. The active job does not make
    // this an ordinary coalescing case: the window is checked first.
    set_due(&mut conn, &fixture, at(2026, 5, 1, 16, 30)).await;
    let outside = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 5, 1, 16, 30),
        fixture.epoch,
    )
    .await
    .unwrap()
    .expect("the occurrence is due");
    assert_eq!(outside.state, OccurrenceState::Skipped);
    assert_eq!(outside.reason.as_deref(), Some("OutsideWindow"));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn clearing_the_window_restores_admission_at_any_time(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    set_window(
        &mut conn,
        &fixture,
        MaintenanceWindow::new(120, 240).unwrap(),
    )
    .await;
    let cleared = schedules::update_schedule(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: Some(None),
            enabled: None,
            expected_revision: 2,
        },
    )
    .await
    .unwrap();
    assert_eq!(cleared.window, None);

    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;
    let occurrence = occurrences::admit_at(&mut conn, fixture.schedule, now, fixture.epoch)
        .await
        .unwrap()
        .expect("the occurrence is due");
    assert_eq!(occurrence.state, OccurrenceState::Admitted);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn clearing_the_window_recomputes_a_future_cursor(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    set_window(
        &mut conn,
        &fixture,
        MaintenanceWindow::new(120, 240).unwrap(),
    )
    .await;

    // An outside-window skip at 10:00 jumps the cursor to tomorrow's opening.
    let skipped_at = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, skipped_at).await;
    let skipped = occurrences::admit_at(&mut conn, fixture.schedule, skipped_at, fixture.epoch)
        .await
        .unwrap()
        .expect("the occurrence is due");
    assert_eq!(skipped.state, OccurrenceState::Skipped);
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 2, 2, 0))
    );

    // Clearing the window at 10:30 must not leave that cursor in place: the
    // 11:00 tick is newly eligible.
    let cleared = schedules::update_schedule_at(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: Some(None),
            enabled: None,
            expected_revision: 2,
        },
        at(2026, 5, 1, 10, 30),
    )
    .await
    .unwrap();
    assert_eq!(cleared.window, None);
    assert_eq!(cleared.revision, 3);
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 1, 11, 0))
    );

    // No manual cursor reset: the recomputed tick admits a job.
    let admitted = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 5, 1, 11, 0),
        fixture.epoch,
    )
    .await
    .unwrap()
    .expect("the occurrence is due");
    assert_eq!(admitted.state, OccurrenceState::Admitted);
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn widening_the_window_recomputes_a_future_cursor(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    set_window(
        &mut conn,
        &fixture,
        MaintenanceWindow::new(120, 240).unwrap(),
    )
    .await;
    let skipped_at = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, skipped_at).await;
    occurrences::admit_at(&mut conn, fixture.schedule, skipped_at, fixture.epoch)
        .await
        .unwrap()
        .expect("the occurrence is due");

    let widened = schedules::update_schedule_at(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: Some(Some(MaintenanceWindow::new(0, 1439).unwrap())),
            enabled: None,
            expected_revision: 2,
        },
        at(2026, 5, 1, 10, 30),
    )
    .await
    .unwrap();
    assert_eq!(
        widened.window,
        Some(MaintenanceWindow::new(0, 1439).unwrap())
    );
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 1, 11, 0))
    );
    let admitted = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 5, 1, 11, 0),
        fixture.epoch,
    )
    .await
    .unwrap()
    .expect("the occurrence is due");
    assert_eq!(admitted.state, OccurrenceState::Admitted);
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_overdue_cursor_survives_a_window_change(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    set_window(
        &mut conn,
        &fixture,
        MaintenanceWindow::new(120, 240).unwrap(),
    )
    .await;
    let due = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, due).await;

    // Clearing at 10:30 leaves the 10:00 evaluation overdue; it must still run
    // under the new window instead of being skipped by a forward re-arm.
    schedules::update_schedule_at(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: Some(None),
            enabled: None,
            expected_revision: 2,
        },
        at(2026, 5, 1, 10, 30),
    )
    .await
    .unwrap();
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(due)
    );
    let admitted = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 5, 1, 10, 30),
        fixture.epoch,
    )
    .await
    .unwrap()
    .expect("the overdue occurrence is due");
    assert_eq!(admitted.state, OccurrenceState::Admitted);
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unchanged_window_keeps_an_advanced_cursor(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let window = MaintenanceWindow::new(120, 240).unwrap();
    set_window(&mut conn, &fixture, window).await;
    let skipped_at = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, skipped_at).await;
    occurrences::admit_at(&mut conn, fixture.schedule, skipped_at, fixture.epoch)
        .await
        .unwrap()
        .expect("the occurrence is due");

    // Rewriting the same window is not a change and must not pull the cursor
    // back into today's outside-window ticks.
    schedules::update_schedule_at(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: Some(Some(window)),
            enabled: None,
            expected_revision: 2,
        },
        at(2026, 5, 1, 10, 30),
    )
    .await
    .unwrap();
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 2, 2, 0))
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_second_fall_back_opening_admits_the_later_valid_run(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 2 * * *", "America/New_York").await;
    // 01:30–04:00 local. 2026-11-01 is the fall-back day.
    set_window(
        &mut conn,
        &fixture,
        MaintenanceWindow::new(90, 240).unwrap(),
    )
    .await;
    // The previous day's 02:00 EDT occurrence is the cursor; 06:10 UTC is the
    // second 01:10 EST, after the first opening and before the second.
    set_due(&mut conn, &fixture, at(2026, 10, 31, 6, 0)).await;
    let skipped = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 11, 1, 6, 10),
        fixture.epoch,
    )
    .await
    .unwrap()
    .expect("the occurrence is due");
    assert_eq!(skipped.state, OccurrenceState::Skipped);
    assert_eq!(skipped.reason.as_deref(), Some("OutsideWindow"));
    // The later 01:30 EST opens the window at 06:30 UTC, so the 02:00 EST run
    // at 07:00 UTC is still ahead today.
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 11, 1, 7, 0))
    );
    let admitted = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 11, 1, 7, 0),
        fixture.epoch,
    )
    .await
    .unwrap()
    .expect("the later run is due");
    assert_eq!(admitted.state, OccurrenceState::Admitted);
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_admission_clock_is_sampled_after_the_schedule_lock(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    set_window(
        &mut conn,
        &fixture,
        MaintenanceWindow::new(120, 240).unwrap(),
    )
    .await;
    // Due at 03:00, inside the 02:00–04:00 window at the pass instant.
    set_due(&mut conn, &fixture, at(2026, 5, 1, 3, 0)).await;
    let application_name = format!("bergsmith-admit-{}", Uuid::now_v7());
    sqlx::query_scalar::<_, String>("SELECT set_config('application_name', $1, false)")
        .bind(&application_name)
        .fetch_one(&mut *conn)
        .await
        .unwrap();

    // Hold the schedule row, so admission cannot take its locks yet.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM maintenance_schedules WHERE id = $1 FOR UPDATE")
        .bind(fixture.schedule.as_uuid())
        .fetch_one(&mut *holder)
        .await
        .unwrap();

    let clock = Arc::new(Mutex::new(at(2026, 5, 1, 3, 30)));
    let (sampled, mut sampled_rx) = mpsc::unbounded_channel();
    let task_clock = Arc::clone(&clock);
    let schedule_id = fixture.schedule;
    let epoch = fixture.epoch;
    let admission = tokio::spawn(async move {
        occurrences::admit_with_clock(&mut conn, schedule_id, epoch, move || {
            let _ = sampled.send(());
            *task_clock.lock().unwrap()
        })
        .await
    });

    // Synchronize on PostgreSQL reporting the admission connection blocked by
    // the holder. A correct implementation cannot have sampled the clock yet.
    wait_until_blocked(&pool, &application_name).await;
    assert!(
        matches!(sampled_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "admission sampled the clock before its schedule lock"
    );

    // The pass began at 03:30 but actually decides at 05:00, after the window
    // closed. It must skip rather than admit inside the stale window.
    *clock.lock().unwrap() = at(2026, 5, 1, 5, 0);
    holder.commit().await.unwrap();

    let occurrence = admission
        .await
        .unwrap()
        .unwrap()
        .expect("the occurrence is due");
    assert_eq!(occurrence.state, OccurrenceState::Skipped);
    assert_eq!(occurrence.reason.as_deref(), Some("OutsideWindow"));
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(jobs_on_table(&mut conn, &fixture.tenant).await, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_window_update_cannot_rewind_past_a_concurrent_admission(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    set_window(
        &mut conn,
        &fixture,
        MaintenanceWindow::new(120, 240).unwrap(),
    )
    .await;
    // The cursor is due at 11:00 while the window is closed.
    set_due(&mut conn, &fixture, at(2026, 5, 1, 11, 0)).await;

    let application_name = format!("bergsmith-update-{}", Uuid::now_v7());
    sqlx::query_scalar::<_, String>("SELECT set_config('application_name', $1, false)")
        .bind(&application_name)
        .fetch_one(&mut *conn)
        .await
        .unwrap();

    // Simulate the admission that wins the race: it holds the schedule lock,
    // records the 11:00 evaluation, advances the cursor to 12:00, and stays
    // open while the update waits.
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM maintenance_schedules WHERE id = $1 FOR UPDATE")
        .bind(fixture.schedule.as_uuid())
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO maintenance_schedule_occurrences
             (id, organization_id, schedule_id, scheduled_for, state, reason)
         VALUES ($1, $2, $3, $4, 'Skipped', 'OutsideWindow')",
    )
    .bind(Uuid::now_v7())
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.schedule.as_uuid())
    .bind(at(2026, 5, 1, 11, 0))
    .execute(&mut *holder)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE maintenance_schedule_state SET next_due_at = $3
         WHERE organization_id = $1 AND schedule_id = $2",
    )
    .bind(fixture.tenant.organization.as_uuid())
    .bind(fixture.schedule.as_uuid())
    .bind(at(2026, 5, 1, 12, 0))
    .execute(&mut *holder)
    .await
    .unwrap();

    // The update starts just before the 11:00 boundary. It blocks on the
    // schedule row; its clock must not be read until it owns the lock.
    let clock = Arc::new(Mutex::new(at(2026, 5, 1, 10, 59)));
    let task_clock = Arc::clone(&clock);
    let schedule_id = fixture.schedule;
    let organization = fixture.tenant.organization;
    let updater = tokio::spawn(async move {
        schedules::update_schedule_with_shared_clock(
            &mut conn,
            organization,
            schedule_id,
            &ScheduleUpdate {
                timing: None,
                window: Some(None),
                enabled: None,
                expected_revision: 2,
            },
            &task_clock,
        )
        .await
    });

    wait_until_blocked(&pool, &application_name).await;
    // The admission's clock is past the boundary now; the update must use this
    // instant, not the 10:59 value it started with.
    *clock.lock().unwrap() = at(2026, 5, 1, 11, 0);
    holder.commit().await.unwrap();

    let updated = updater.await.unwrap().unwrap();
    assert_eq!(updated.window, None);
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 1, 12, 0))
    );

    // A rewound cursor would replay the already-recorded 11:00 occurrence and
    // fail its uniqueness constraint instead of returning "not due".
    assert!(
        occurrences::admit_at(
            &mut conn,
            fixture.schedule,
            at(2026, 5, 1, 11, 0),
            fixture.epoch,
        )
        .await
        .unwrap()
        .is_none()
    );
    let admitted = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 5, 1, 12, 0),
        fixture.epoch,
    )
    .await
    .unwrap()
    .expect("the next occurrence is due");
    assert_eq!(admitted.state, OccurrenceState::Admitted);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_skewed_update_clock_cannot_rewind_onto_a_recorded_occurrence(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    set_window(
        &mut conn,
        &fixture,
        MaintenanceWindow::new(120, 240).unwrap(),
    )
    .await;
    set_due(&mut conn, &fixture, at(2026, 5, 1, 11, 0)).await;

    // Admission at coordinator time 11:00 records the evaluation and, because
    // the window is closed, advances the cursor to tomorrow's opening.
    let recorded = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 5, 1, 11, 0),
        fixture.epoch,
    )
    .await
    .unwrap()
    .expect("the occurrence is due");
    assert_eq!(recorded.state, OccurrenceState::Skipped);
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 2, 2, 0))
    );

    // The window update samples a database clock just behind the admission's
    // coordinator clock. It must select 12:00, the first occurrence after the
    // recorded 11:00, rather than replaying 11:00. The cron boundary is
    // minute-granular, so 10:59 stands in for 10:59:59.900 on an hourly
    // schedule.
    let updated = schedules::update_schedule_at(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: None,
            window: Some(None),
            enabled: None,
            expected_revision: 2,
        },
        at(2026, 5, 1, 10, 59),
    )
    .await
    .unwrap();
    assert_eq!(updated.window, None);
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 1, 12, 0))
    );

    // A rewound cursor would replay the recorded 11:00 occurrence and fail its
    // uniqueness constraint instead of returning "not due".
    assert!(
        occurrences::admit_at(
            &mut conn,
            fixture.schedule,
            at(2026, 5, 1, 11, 0),
            fixture.epoch,
        )
        .await
        .unwrap()
        .is_none()
    );
    let next = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 5, 1, 12, 0),
        fixture.epoch,
    )
    .await
    .unwrap()
    .expect("the next occurrence is due");
    assert_eq!(next.state, OccurrenceState::Admitted);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_skewed_timing_change_cannot_rearm_to_a_recorded_occurrence(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    set_due(&mut conn, &fixture, at(2026, 5, 1, 11, 0)).await;

    // Admission at coordinator time 11:00 records the occurrence and advances
    // the cursor to 12:00.
    let recorded = occurrences::admit_at(
        &mut conn,
        fixture.schedule,
        at(2026, 5, 1, 11, 0),
        fixture.epoch,
    )
    .await
    .unwrap()
    .expect("the occurrence is due");
    assert_eq!(recorded.state, OccurrenceState::Admitted);

    // A timing edit re-arms from the database clock, which is behind the
    // admission's coordinator clock. It must not select the recorded instant.
    let updated = schedules::update_schedule_at(
        &mut conn,
        fixture.tenant.organization,
        fixture.schedule,
        &ScheduleUpdate {
            timing: Some(CronSchedule::parse("0 * * * *", "UTC").unwrap()),
            window: None,
            enabled: None,
            expected_revision: 1,
        },
        at(2026, 5, 1, 10, 59),
    )
    .await
    .unwrap();
    assert_eq!(updated.revision, 2);
    assert_eq!(
        occurrences::next_due(&mut conn, fixture.tenant.organization, fixture.schedule)
            .await
            .unwrap(),
        Some(at(2026, 5, 1, 12, 0))
    );
    // A rewound cursor would collide with the recorded 11:00 occurrence.
    assert!(
        occurrences::admit_at(
            &mut conn,
            fixture.schedule,
            at(2026, 5, 1, 11, 0),
            fixture.epoch,
        )
        .await
        .unwrap()
        .is_none()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn history_reads_newest_first_and_continues_before_an_instant(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let current = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    set_due(&mut conn, &current, at(2026, 5, 1, 10, 0)).await;
    let admitted = occurrences::admit_at(
        &mut conn,
        current.schedule,
        at(2026, 5, 1, 10, 0),
        current.epoch,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(admitted.state, OccurrenceState::Admitted);

    // The admitted job keeps the table, so the later evaluations coalesce.
    let second = occurrences::admit_at(
        &mut conn,
        current.schedule,
        at(2026, 5, 1, 11, 0),
        current.epoch,
    )
    .await
    .unwrap()
    .unwrap();
    let third = occurrences::admit_at(
        &mut conn,
        current.schedule,
        at(2026, 5, 1, 12, 0),
        current.epoch,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(second.state, OccurrenceState::Coalesced);
    assert_eq!(third.state, OccurrenceState::Coalesced);

    let page = occurrences::occurrences_before(
        &mut conn,
        current.tenant.organization,
        current.schedule,
        None,
        2,
    )
    .await
    .unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].scheduled_for, at(2026, 5, 1, 12, 0));
    assert_eq!(page[1].scheduled_for, at(2026, 5, 1, 11, 0));

    // The cursor is the last instant read, so the page continues before it.
    let rest = occurrences::occurrences_before(
        &mut conn,
        current.tenant.organization,
        current.schedule,
        Some(page[1].scheduled_for),
        2,
    )
    .await
    .unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].scheduled_for, at(2026, 5, 1, 10, 0));
    assert_eq!(rest[0].state, OccurrenceState::Admitted);

    // Another schedule's history is never mixed in.
    let empty = occurrences::occurrences_before(
        &mut conn,
        current.tenant.organization,
        ScheduleId::generate(),
        None,
        10,
    )
    .await
    .unwrap();
    assert!(empty.is_empty());
}
