//! Maintenance policies and schedules against real PostgreSQL (BS-019.T1).
#![expect(clippy::unwrap_used, reason = "integration setup and assertions")]

mod support;

use bergsmith_common::ids::{OrganizationId, PolicyId, PrincipalId, ScheduleId};
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_common::policy::{MaintenanceOperation, PolicySpec};
use bergsmith_common::schedule::CronSchedule;
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::schedules::{
    self, NewMaintenancePolicy, NewMaintenanceSchedule, PolicyUpdate, ScheduleUpdate,
};
use bergsmith_persistence::tenancy::{self, NewPrincipal, PrincipalKind};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

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

async fn grant_role(
    conn: &mut PgConnection,
    organization: OrganizationId,
    principal: PrincipalId,
    role: Role,
    scope: GrantScope,
) {
    grants::create_grant(
        conn,
        &NewRoleGrant {
            id: bergsmith_common::ids::RoleGrantId::generate(),
            organization_id: organization,
            principal_id: principal,
            role,
            scope,
            granted_by: None,
        },
    )
    .await
    .unwrap();
}

async fn policy(
    conn: &mut PgConnection,
    organization: OrganizationId,
    created_by: PrincipalId,
    name: &str,
) -> PolicyId {
    let id = PolicyId::generate();
    schedules::create_policy(
        conn,
        &NewMaintenancePolicy {
            id,
            organization_id: organization,
            name: name.to_owned(),
            specification: PolicySpec::default(),
            created_by,
        },
    )
    .await
    .unwrap();
    id
}

async fn schedule(
    conn: &mut PgConnection,
    tenant: &support::Tenant,
    policy_id: PolicyId,
    service: PrincipalId,
) -> ScheduleId {
    let id = ScheduleId::generate();
    schedules::create_schedule(
        conn,
        &NewMaintenanceSchedule {
            id,
            organization_id: tenant.organization,
            policy_id,
            table_id: tenant.table,
            timing: CronSchedule::parse("0 2 * * *", "Europe/Berlin").unwrap(),
            service_principal_id: service,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();
    id
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn policies_round_trip_and_list_in_identifier_order(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let first = policy(&mut conn, tenant.organization, tenant.user, "nightly").await;
    let second = policy(&mut conn, tenant.organization, tenant.user, "weekly").await;

    let stored = schedules::policy(&mut conn, tenant.organization, first)
        .await
        .unwrap();
    assert_eq!(stored.id, first);
    assert_eq!(stored.name, "nightly");
    assert_eq!(stored.specification, PolicySpec::default());
    assert!(stored.enabled);
    assert_eq!(stored.revision, 1);
    assert_eq!(stored.created_by, tenant.user);

    let listed = schedules::policies(&mut conn, tenant.organization, None, 10)
        .await
        .unwrap();
    assert_eq!(
        listed.iter().map(|policy| policy.id).collect::<Vec<_>>(),
        vec![first, second]
    );
    let after_first = schedules::policies(&mut conn, tenant.organization, Some(first), 10)
        .await
        .unwrap();
    assert_eq!(
        after_first
            .iter()
            .map(|policy| policy.id)
            .collect::<Vec<_>>(),
        vec![second]
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn policy_names_are_unique_within_an_organization(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    policy(&mut conn, tenant.organization, tenant.user, "nightly").await;

    let error = schedules::create_policy(
        &mut conn,
        &NewMaintenancePolicy {
            id: PolicyId::generate(),
            organization_id: tenant.organization,
            name: "nightly".to_owned(),
            specification: PolicySpec::default(),
            created_by: tenant.user,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PersistenceError::AlreadyExists { .. }));

    // Another organization may use the same name.
    let other = support::tenant(&mut conn, "other").await;
    policy(&mut conn, other.organization, other.user, "nightly").await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn policies_are_invisible_to_other_organizations(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other = support::tenant(&mut conn, "other").await;
    let id = policy(&mut conn, tenant.organization, tenant.user, "nightly").await;

    let error = schedules::policy(&mut conn, other.organization, id)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::NotFound {
            entity: "maintenance policy",
            ..
        }
    ));
    assert!(
        schedules::policies(&mut conn, other.organization, None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn policy_updates_are_guarded_by_the_revision(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let id = policy(&mut conn, tenant.organization, tenant.user, "nightly").await;

    let specification =
        PolicySpec::new(MaintenanceOperation::Compact, 512 * 1024 * 1024, 8).unwrap();
    let updated = schedules::update_policy(
        &mut conn,
        tenant.organization,
        id,
        &PolicyUpdate {
            name: Some("nightly-small-files".to_owned()),
            specification: Some(specification),
            enabled: Some(false),
            expected_revision: 1,
        },
    )
    .await
    .unwrap();
    assert_eq!(updated.name, "nightly-small-files");
    assert_eq!(updated.specification, specification);
    assert!(!updated.enabled);
    assert_eq!(updated.revision, 2);
    assert!(updated.updated_at >= updated.created_at);

    let error = schedules::update_policy(
        &mut conn,
        tenant.organization,
        id,
        &PolicyUpdate {
            name: None,
            specification: None,
            enabled: Some(true),
            expected_revision: 1,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::ConfigRevisionConflict {
            entity: "maintenance policy",
            expected: 1,
            actual: 2,
            ..
        }
    ));

    let error = schedules::update_policy(
        &mut conn,
        tenant.organization,
        PolicyId::generate(),
        &PolicyUpdate {
            name: None,
            specification: None,
            enabled: Some(true),
            expected_revision: 1,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::NotFound {
            entity: "maintenance policy",
            ..
        }
    ));

    // An update without a replacement specification keeps the stored one.
    let reenabled = schedules::update_policy(
        &mut conn,
        tenant.organization,
        id,
        &PolicyUpdate {
            name: None,
            specification: None,
            enabled: Some(true),
            expected_revision: 2,
        },
    )
    .await
    .unwrap();
    assert!(reenabled.enabled);
    assert_eq!(reenabled.specification, specification);
    assert_eq!(reenabled.revision, 3);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn schedules_require_a_service_identity_with_a_current_grant(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let policy_id = policy(&mut conn, tenant.organization, tenant.user, "nightly").await;

    // A person cannot be the execution identity, even with a grant.
    let error = schedules::create_schedule(
        &mut conn,
        &NewMaintenanceSchedule {
            id: ScheduleId::generate(),
            organization_id: tenant.organization,
            policy_id,
            table_id: tenant.table,
            timing: CronSchedule::parse("0 2 * * *", "UTC").unwrap(),
            service_principal_id: tenant.user,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::IneligibleServiceIdentity { .. }
    ));

    // A service account without a grant cannot run either.
    let unprivileged = service_principal(&mut conn, tenant.organization, "unprivileged").await;
    let error = schedules::create_schedule(
        &mut conn,
        &NewMaintenanceSchedule {
            id: ScheduleId::generate(),
            organization_id: tenant.organization,
            policy_id,
            table_id: tenant.table,
            timing: CronSchedule::parse("0 2 * * *", "UTC").unwrap(),
            service_principal_id: unprivileged,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::IneligibleServiceIdentity { .. }
    ));

    // A grant that does not cover the table is not enough.
    let elsewhere = service_principal(&mut conn, tenant.organization, "elsewhere").await;
    let other_table = bergsmith_persistence::tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(tenant.connection, "other"),
        Uuid::now_v7(),
    )
    .await
    .unwrap()
    .id;
    grant_role(
        &mut conn,
        tenant.organization,
        elsewhere,
        Role::MaintenanceService,
        GrantScope::table(other_table),
    )
    .await;
    let error = schedules::create_schedule(
        &mut conn,
        &NewMaintenanceSchedule {
            id: ScheduleId::generate(),
            organization_id: tenant.organization,
            policy_id,
            table_id: tenant.table,
            timing: CronSchedule::parse("0 2 * * *", "UTC").unwrap(),
            service_principal_id: elsewhere,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::IneligibleServiceIdentity { .. }
    ));

    // A table-scoped maintenance grant covers exactly this table.
    let eligible = service_principal(&mut conn, tenant.organization, "scheduler").await;
    grant_role(
        &mut conn,
        tenant.organization,
        eligible,
        Role::MaintenanceService,
        GrantScope::table(tenant.table),
    )
    .await;
    schedule(&mut conn, &tenant, policy_id, eligible).await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_policy_runs_at_most_once_per_table(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let policy_id = policy(&mut conn, tenant.organization, tenant.user, "nightly").await;
    let service = service_principal(&mut conn, tenant.organization, "scheduler").await;
    grant_role(
        &mut conn,
        tenant.organization,
        service,
        Role::MaintenanceService,
        GrantScope::organization(),
    )
    .await;
    schedule(&mut conn, &tenant, policy_id, service).await;

    let error = schedules::create_schedule(
        &mut conn,
        &NewMaintenanceSchedule {
            id: ScheduleId::generate(),
            organization_id: tenant.organization,
            policy_id,
            table_id: tenant.table,
            timing: CronSchedule::parse("0 3 * * *", "UTC").unwrap(),
            service_principal_id: service,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PersistenceError::AlreadyExists { .. }));

    // A second policy for the same table is a different schedule.
    let second_policy = policy(&mut conn, tenant.organization, tenant.user, "hourly").await;
    schedule(&mut conn, &tenant, second_policy, service).await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn schedules_round_trip_and_update_only_timing_and_enabled(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let policy_id = policy(&mut conn, tenant.organization, tenant.user, "nightly").await;
    let service = service_principal(&mut conn, tenant.organization, "scheduler").await;
    grant_role(
        &mut conn,
        tenant.organization,
        service,
        Role::MaintenanceService,
        GrantScope::organization(),
    )
    .await;
    let id = schedule(&mut conn, &tenant, policy_id, service).await;

    let stored = schedules::schedule(&mut conn, tenant.organization, id)
        .await
        .unwrap();
    assert_eq!(stored.policy_id, policy_id);
    assert_eq!(stored.table_id, tenant.table);
    assert_eq!(stored.service_principal_id, service);
    assert_eq!(
        stored.timing,
        CronSchedule::parse("0 2 * * *", "Europe/Berlin").unwrap()
    );
    assert!(stored.enabled);
    assert_eq!(stored.revision, 1);

    let timing = CronSchedule::parse("30 1 * * MON-FRI", "UTC").unwrap();
    let updated = schedules::update_schedule(
        &mut conn,
        tenant.organization,
        id,
        &ScheduleUpdate {
            timing: Some(timing.clone()),
            enabled: Some(false),
            expected_revision: 1,
        },
    )
    .await
    .unwrap();
    assert_eq!(updated.timing, timing);
    assert!(!updated.enabled);
    assert_eq!(updated.revision, 2);
    assert_eq!(updated.policy_id, policy_id);
    assert_eq!(updated.table_id, tenant.table);

    let stale = schedules::update_schedule(
        &mut conn,
        tenant.organization,
        id,
        &ScheduleUpdate {
            timing: None,
            enabled: Some(true),
            expected_revision: 1,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        stale,
        PersistenceError::ConfigRevisionConflict {
            entity: "maintenance schedule",
            expected: 1,
            actual: 2,
            ..
        }
    ));

    // An update that changes only the enabled flag keeps the timing contract.
    let reenabled = schedules::update_schedule(
        &mut conn,
        tenant.organization,
        id,
        &ScheduleUpdate {
            timing: None,
            enabled: Some(true),
            expected_revision: 2,
        },
    )
    .await
    .unwrap();
    assert!(reenabled.enabled);
    assert_eq!(reenabled.timing, timing);
    assert_eq!(reenabled.revision, 3);

    let listed = schedules::schedules(&mut conn, tenant.organization, None, 10)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn stored_timing_cannot_be_changed_to_an_invalid_value_or_corrupted(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let policy_id = policy(&mut conn, tenant.organization, tenant.user, "nightly").await;
    let service = service_principal(&mut conn, tenant.organization, "scheduler").await;
    grant_role(
        &mut conn,
        tenant.organization,
        service,
        Role::MaintenanceService,
        GrantScope::organization(),
    )
    .await;
    let id = schedule(&mut conn, &tenant, policy_id, service).await;

    // The timing contract is validated before insert, so a corrupt row can
    // only appear through a schema bypass. Reading one must fail loudly.
    sqlx::query("UPDATE maintenance_schedules SET cron_expression = 'not a cron' WHERE id = $1")
        .bind(id.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let error = schedules::schedule(&mut conn, tenant.organization, id)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::CorruptValue {
            entity: "schedule timing",
            ..
        }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn stored_policies_with_unknown_operations_fail_to_decode(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let id = policy(&mut conn, tenant.organization, tenant.user, "nightly").await;

    // Only a schema bypass can produce this row; reading it must not guess.
    sqlx::query(
        "ALTER TABLE maintenance_policies DROP CONSTRAINT maintenance_policies_operation_check",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("UPDATE maintenance_policies SET operation = 'Expire' WHERE id = $1")
        .bind(id.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let error = schedules::policy(&mut conn, tenant.organization, id)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::CorruptValue {
            entity: "maintenance operation",
            ..
        }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_schema_bounds_policy_parameters(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;

    let error = sqlx::query(
        "INSERT INTO maintenance_policies
             (id, organization_id, name, operation, target_file_size_bytes,
              min_input_files, created_by)
         VALUES ($1, $2, 'too-small', 'Compact', 1, 5, $3)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant.organization.as_uuid())
    .bind(tenant.user.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap_err();
    assert_eq!(support::sqlstate(&error), "23514");

    let error = sqlx::query(
        "INSERT INTO maintenance_policies
             (id, organization_id, name, operation, target_file_size_bytes,
              min_input_files, created_by)
         VALUES ($1, $2, 'unknown-operation', 'Expire', 1048576, 5, $3)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant.organization.as_uuid())
    .bind(tenant.user.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap_err();
    assert_eq!(support::sqlstate(&error), "23514");
}
