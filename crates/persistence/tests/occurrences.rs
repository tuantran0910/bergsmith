//! Schedule occurrence admission against real PostgreSQL (BS-019.T2).
#![expect(clippy::unwrap_used, reason = "integration setup and assertions")]

mod support;

use bergsmith_common::ids::{OrganizationId, PolicyId, PrincipalId, RoleGrantId, ScheduleId};
use bergsmith_common::job::JobState;
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_common::policy::PolicySpec;
use bergsmith_common::schedule::CronSchedule;
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
use uuid::Uuid;

struct Fixture {
    tenant: support::Tenant,
    service: PrincipalId,
    grant: RoleGrantId,
    policy: PolicyId,
    schedule: ScheduleId,
    epoch: i64,
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

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_due_occurrence_admits_one_job_as_the_service_identity(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixture = fixture(&mut conn, "acme", "0 * * * *", "UTC").await;
    let now = at(2026, 5, 1, 10, 0);
    set_due(&mut conn, &fixture, now).await;

    let occurrence = occurrences::admit(&mut conn, fixture.schedule, now, fixture.epoch)
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
        occurrences::admit(&mut conn, fixture.schedule, now, fixture.epoch)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        occurrences::admit(&mut conn, fixture.schedule, now, fixture.epoch)
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
    let occurrence = occurrences::admit(&mut conn, fixture.schedule, now, fixture.epoch)
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

    let occurrence = occurrences::admit(&mut conn, fixture.schedule, now, fixture.epoch)
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

    let occurrence = occurrences::admit(&mut conn, fixture.schedule, now, fixture.epoch)
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

    let occurrence = occurrences::admit(&mut conn, fixture.schedule, now, fixture.epoch)
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
        occurrences::admit(
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
    let occurrence = occurrences::admit(&mut conn, fixture.schedule, later, fixture.epoch)
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
    occurrences::admit(
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
    occurrences::admit(
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
    let occurrence = occurrences::admit(&mut conn, fixture.schedule, now, fixture.epoch)
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
    let occurrence = occurrences::admit(&mut conn, fixture.schedule, now, fixture.epoch)
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
    let occurrence = occurrences::admit(&mut conn, fixture.schedule, now, fixture.epoch)
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
    let fresh = occurrences::admit(&mut conn, fixture.schedule, next, fixture.epoch)
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
    let occurrence = occurrences::admit(&mut conn, fixture.schedule, now, fixture.epoch)
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
