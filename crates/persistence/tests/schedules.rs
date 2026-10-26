//! Maintenance policies and schedules against real PostgreSQL (BS-019.T1).
#![expect(clippy::unwrap_used, reason = "integration setup and assertions")]

mod support;

use bergsmith_common::health::{
    FindingKind, HealthThresholds, HealthTrigger, Severity, Thresholds,
};
use bergsmith_common::ids::{OrganizationId, PolicyId, PrincipalId, ScheduleId};
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_common::policy::{MaintenanceOperation, PolicySpec};
use bergsmith_common::schedule::CronSchedule;
use bergsmith_common::window::MaintenanceWindow;
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

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn policies_carry_health_thresholds_and_a_trigger(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let thresholds = HealthThresholds {
        small_files: Thresholds {
            warning: 64 * 1024 * 1024,
            critical: 16 * 1024 * 1024,
        },
        ..HealthThresholds::default()
    };
    let trigger = HealthTrigger::new(FindingKind::SmallFiles, Severity::Critical);
    let id = PolicyId::generate();
    let created = schedules::create_policy_with_health(
        &mut conn,
        &NewMaintenancePolicy {
            id,
            organization_id: tenant.organization,
            name: "health".to_owned(),
            specification: PolicySpec::default(),
            created_by: tenant.user,
        },
        &thresholds,
        Some(trigger),
    )
    .await
    .unwrap();
    assert_eq!(created.health_thresholds, thresholds);
    assert_eq!(created.health_trigger, Some(trigger));

    // A provided threshold set replaces the stored one, and a provided null
    // trigger clears it; absent values would leave both unchanged.
    let replacement = HealthThresholds::default();
    let updated = schedules::update_policy_with_health(
        &mut conn,
        tenant.organization,
        id,
        &PolicyUpdate {
            name: None,
            specification: None,
            enabled: None,
            expected_revision: created.revision,
        },
        Some(replacement),
        Some(None),
    )
    .await
    .unwrap();
    assert_eq!(updated.health_thresholds, replacement);
    assert_eq!(updated.health_trigger, None);
    assert_eq!(updated.revision, created.revision + 1);

    // An inverted ladder is refused on create and on update.
    let inverted = HealthThresholds {
        small_files: Thresholds {
            warning: 16 * 1024 * 1024,
            critical: 64 * 1024 * 1024,
        },
        ..HealthThresholds::default()
    };
    assert!(matches!(
        schedules::create_policy_with_health(
            &mut conn,
            &NewMaintenancePolicy {
                id: PolicyId::generate(),
                organization_id: tenant.organization,
                name: "inverted".to_owned(),
                specification: PolicySpec::default(),
                created_by: tenant.user,
            },
            &inverted,
            None,
        )
        .await,
        Err(PersistenceError::InvalidValue { .. })
    ));
    assert!(matches!(
        schedules::update_policy_with_health(
            &mut conn,
            tenant.organization,
            id,
            &PolicyUpdate {
                name: None,
                specification: None,
                enabled: None,
                expected_revision: updated.revision,
            },
            Some(inverted),
            None,
        )
        .await,
        Err(PersistenceError::InvalidValue { .. })
    ));
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
            window: None,
            service_principal_id: service,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();
    id
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn enabled_thresholds_are_read_per_table_and_skip_disabled_configuration(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other = support::tenant(&mut conn, "other").await;
    let service = service_principal(&mut conn, tenant.organization, "scheduler").await;
    grant_role(
        &mut conn,
        tenant.organization,
        service,
        Role::MaintenanceService,
        GrantScope::table(tenant.table),
    )
    .await;
    let thresholds = HealthThresholds {
        small_files: Thresholds {
            warning: 64 * 1024 * 1024,
            critical: 16 * 1024 * 1024,
        },
        ..HealthThresholds::default()
    };
    let strict = PolicyId::generate();
    schedules::create_policy_with_health(
        &mut conn,
        &NewMaintenancePolicy {
            id: strict,
            organization_id: tenant.organization,
            name: "strict".to_owned(),
            specification: PolicySpec::default(),
            created_by: tenant.user,
        },
        &thresholds,
        None,
    )
    .await
    .unwrap();
    let lenient = policy(&mut conn, tenant.organization, tenant.user, "lenient").await;
    schedule(&mut conn, &tenant, strict, service).await;
    schedule(&mut conn, &tenant, lenient, service).await;

    // Both enabled policies are read for the table, with the stored set.
    let sets =
        schedules::enabled_thresholds_for_tables(&mut conn, tenant.organization, &[tenant.table])
            .await
            .unwrap();
    assert_eq!(sets.len(), 2);
    let strict_set = sets.iter().find(|set| set.policy_id == strict).unwrap();
    assert_eq!(strict_set.table_id, tenant.table);
    assert_eq!(strict_set.policy_name, "strict");
    assert_eq!(strict_set.thresholds().unwrap(), thresholds);
    let lenient_set = sets.iter().find(|set| set.policy_id == lenient).unwrap();
    assert_eq!(
        lenient_set.thresholds().unwrap(),
        HealthThresholds::default()
    );

    // A disabled policy admits nothing, so its set is omitted.
    schedules::update_policy_with_health(
        &mut conn,
        tenant.organization,
        strict,
        &PolicyUpdate {
            name: None,
            specification: None,
            enabled: Some(false),
            expected_revision: 1,
        },
        None,
        None,
    )
    .await
    .unwrap();
    let sets =
        schedules::enabled_thresholds_for_tables(&mut conn, tenant.organization, &[tenant.table])
            .await
            .unwrap();
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0].policy_id, lenient);

    // Another organization's tables are not read, and neither is a table
    // without a schedule.
    assert!(
        schedules::enabled_thresholds_for_tables(&mut conn, other.organization, &[tenant.table])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        schedules::enabled_thresholds_for_tables(&mut conn, tenant.organization, &[other.table])
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn stored_threshold_sets_decode_on_demand_and_reject_corruption(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let service = service_principal(&mut conn, tenant.organization, "scheduler").await;
    grant_role(
        &mut conn,
        tenant.organization,
        service,
        Role::MaintenanceService,
        GrantScope::table(tenant.table),
    )
    .await;
    let thresholds = HealthThresholds {
        small_files: Thresholds {
            warning: 64 * 1024 * 1024,
            critical: 16 * 1024 * 1024,
        },
        ..HealthThresholds::default()
    };
    let policy_id = PolicyId::generate();
    schedules::create_policy_with_health(
        &mut conn,
        &NewMaintenancePolicy {
            id: policy_id,
            organization_id: tenant.organization,
            name: "strict".to_owned(),
            specification: PolicySpec::default(),
            created_by: tenant.user,
        },
        &thresholds,
        None,
    )
    .await
    .unwrap();
    schedule(&mut conn, &tenant, policy_id, service).await;
    let sets =
        schedules::enabled_thresholds_for_tables(&mut conn, tenant.organization, &[tenant.table])
            .await
            .unwrap();
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0].thresholds().unwrap(), thresholds);

    // A malformed stored set is reported by the decode, not by the read.
    sqlx::query(
        r#"UPDATE maintenance_policies SET health_thresholds =
           '{"small_files": {"warning": "high"}}'::jsonb WHERE id = $1"#,
    )
    .bind(policy_id.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let sets =
        schedules::enabled_thresholds_for_tables(&mut conn, tenant.organization, &[tenant.table])
            .await
            .unwrap();
    assert_eq!(sets.len(), 1);
    assert!(matches!(
        sets[0].thresholds(),
        Err(PersistenceError::CorruptValue { .. })
    ));

    // An inverted ladder fails validation the same way.
    sqlx::query(
        r#"UPDATE maintenance_policies SET health_thresholds =
           '{"small_files": {"warning": 1024, "critical": 1048576}}'::jsonb
           WHERE id = $1"#,
    )
    .bind(policy_id.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let sets =
        schedules::enabled_thresholds_for_tables(&mut conn, tenant.organization, &[tenant.table])
            .await
            .unwrap();
    assert!(matches!(
        sets[0].thresholds(),
        Err(PersistenceError::CorruptValue { .. })
    ));
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

    let specification = PolicySpec::new(
        MaintenanceOperation::Compact,
        512 * 1024 * 1024,
        8,
        PolicySpec::MAX_RUN_INPUT_BYTES,
    )
    .unwrap();
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
            window: None,
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
            window: None,
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
            window: None,
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
            window: None,
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
            window: None,
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
            window: None,
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
            window: None,
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

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn policies_store_and_update_the_per_run_input_budget(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let id = PolicyId::generate();
    let specification = PolicySpec::new(
        MaintenanceOperation::Compact,
        256 * 1024 * 1024,
        5,
        8 * 1024 * 1024 * 1024,
    )
    .unwrap();
    schedules::create_policy(
        &mut conn,
        &NewMaintenancePolicy {
            id,
            organization_id: tenant.organization,
            name: "bounded".to_owned(),
            specification,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();

    let stored = schedules::policy(&mut conn, tenant.organization, id)
        .await
        .unwrap();
    assert_eq!(stored.specification.max_input_bytes, 8 * 1024 * 1024 * 1024);

    let tighter = PolicySpec::new(
        MaintenanceOperation::Compact,
        256 * 1024 * 1024,
        5,
        2 * 1024 * 1024 * 1024,
    )
    .unwrap();
    let updated = schedules::update_policy(
        &mut conn,
        tenant.organization,
        id,
        &PolicyUpdate {
            name: None,
            specification: Some(tighter),
            enabled: None,
            expected_revision: 1,
        },
    )
    .await
    .unwrap();
    assert_eq!(updated.revision, 2);
    assert_eq!(
        updated.specification.max_input_bytes,
        2 * 1024 * 1024 * 1024
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_schema_bounds_the_per_run_input_budget(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;

    for (target, budget) in [(1_048_576_i64, 0_i64), (2_097_152, 1_048_576)] {
        let error = sqlx::query(
            "INSERT INTO maintenance_policies
                 (id, organization_id, name, operation, target_file_size_bytes,
                  min_input_files, max_input_bytes, created_by)
             VALUES ($1, $2, 'bad-budget', 'Compact', $3, 5, $4, $5)",
        )
        .bind(Uuid::now_v7())
        .bind(tenant.organization.as_uuid())
        .bind(target)
        .bind(budget)
        .bind(tenant.user.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap_err();
        assert_eq!(support::sqlstate(&error), "23514", "{target} {budget}");
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn schedules_store_replace_and_clear_their_window(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let service = service_principal(&mut conn, tenant.organization, "scheduler").await;
    grant_role(
        &mut conn,
        tenant.organization,
        service,
        Role::MaintenanceService,
        GrantScope::table(tenant.table),
    )
    .await;
    let policy_id = policy(&mut conn, tenant.organization, tenant.user, "nightly").await;

    let id = ScheduleId::generate();
    let created = schedules::create_schedule(
        &mut conn,
        &NewMaintenanceSchedule {
            id,
            organization_id: tenant.organization,
            policy_id,
            table_id: tenant.table,
            timing: CronSchedule::parse("0 23 * * *", "UTC").unwrap(),
            window: Some(MaintenanceWindow::new(1320, 120).unwrap()),
            service_principal_id: service,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        created.window,
        Some(MaintenanceWindow::new(1320, 120).unwrap())
    );

    let replaced = schedules::update_schedule(
        &mut conn,
        tenant.organization,
        id,
        &ScheduleUpdate {
            timing: None,
            window: Some(Some(MaintenanceWindow::new(60, 120).unwrap())),
            enabled: None,
            expected_revision: 1,
        },
    )
    .await
    .unwrap();
    assert_eq!(replaced.revision, 2);
    assert_eq!(
        replaced.window,
        Some(MaintenanceWindow::new(60, 120).unwrap())
    );

    // `None` leaves the stored window alone, including after a timing edit.
    let untouched = schedules::update_schedule(
        &mut conn,
        tenant.organization,
        id,
        &ScheduleUpdate {
            timing: Some(CronSchedule::parse("0 1 * * *", "UTC").unwrap()),
            window: None,
            enabled: None,
            expected_revision: 2,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        untouched.window,
        Some(MaintenanceWindow::new(60, 120).unwrap())
    );

    let cleared = schedules::update_schedule(
        &mut conn,
        tenant.organization,
        id,
        &ScheduleUpdate {
            timing: None,
            window: Some(None),
            enabled: None,
            expected_revision: 3,
        },
    )
    .await
    .unwrap();
    assert_eq!(cleared.window, None);
    assert_eq!(cleared.revision, 4);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_schema_bounds_schedule_windows(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let service = service_principal(&mut conn, tenant.organization, "scheduler").await;
    let policy_id = policy(&mut conn, tenant.organization, tenant.user, "nightly").await;

    for (start, end) in [
        (Some(60_i32), None),
        (Some(60), Some(60)),
        (Some(1440), Some(60)),
        (Some(-1), Some(60)),
    ] {
        let error = sqlx::query(
            "INSERT INTO maintenance_schedules
                 (id, organization_id, policy_id, table_identity_id, cron_expression,
                  time_zone, window_start_minute, window_end_minute,
                  service_principal_id, created_by)
             VALUES ($1, $2, $3, $4, '0 2 * * *', 'UTC', $5, $6, $7, $8)",
        )
        .bind(Uuid::now_v7())
        .bind(tenant.organization.as_uuid())
        .bind(policy_id.as_uuid())
        .bind(tenant.table.as_uuid())
        .bind(start)
        .bind(end)
        .bind(service.as_uuid())
        .bind(tenant.user.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap_err();
        assert_eq!(support::sqlstate(&error), "23514", "{start:?} {end:?}");
    }
}
