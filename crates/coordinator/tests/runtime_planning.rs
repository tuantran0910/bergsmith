//! The serve runtime turns a `Planning` job into a registered dispatch (BS-020.T1).

// Fixtures fail the test immediately; helper functions are not `#[test]` items,
// so clippy's allow-*-in-tests settings do not cover them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bergsmith_catalog::{ObservedTable, TableName};
use bergsmith_common::ids::{CatalogConnectionId, JobId, OrganizationId, PrincipalId, RoleGrantId};
use bergsmith_common::job::JobState;
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_coordinator::inventory::record_observation;
use bergsmith_coordinator::runtime::catalogs::{RuntimeCatalogs, RuntimeWorkerAccess};
use bergsmith_coordinator::runtime::config::{RuntimeProfiles, TaskSettings};
use bergsmith_coordinator::runtime::planning::PlannerService;
use bergsmith_coordinator::scheduling::{Scheduler, SchedulerConfig};
use bergsmith_coordinator::workers::{WorkerConfig, WorkerService};
use bergsmith_fixtures::{Commit, FileSpec, TableSpec, row};
use bergsmith_maintenance::PlannerConfig;
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::jobs::{self, NewJob};
use bergsmith_persistence::scheduling::RetryPolicy;
use bergsmith_persistence::tenancy::{
    self, NewCatalogConnection, NewOrganization, NewPrincipal, PrincipalKind,
};
use bergsmith_persistence::{Database, DatabaseConfig};
use bergsmith_storage::{Location, StorageProfile, StorageProfiles};
use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
use sqlx::{ConnectOptions, PgPool};

async fn fixture() -> bergsmith_fixtures::Fixture {
    let schema = Schema::builder()
        .with_fields([Arc::new(NestedField::required(
            1,
            "id",
            Type::Primitive(PrimitiveType::Long),
        ))])
        .build()
        .unwrap();
    let mut commit = Commit::new();
    for index in 0..5_i64 {
        commit = commit.file(FileSpec::data(format!("d{index}"), [row![1 => index]]));
    }
    TableSpec::new(schema)
        .commit(commit)
        .generate()
        .await
        .unwrap()
}

fn database_url(pool: &PgPool) -> String {
    pool.connect_options().to_url_lossy().to_string()
}

async fn runtime_epoch(conn: &mut sqlx::PgConnection) -> i64 {
    let acquired = bergsmith_persistence::leadership::acquire_leadership(
        conn,
        "runtime-test",
        std::time::Duration::from_secs(60),
    )
    .await
    .expect("leadership query succeeds");
    acquired
        .expect("test database has no active coordinator")
        .epoch
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_planning_pass_records_a_plan_and_registers_dispatch(pool: PgPool) {
    let fixture = fixture().await;
    let mut server = mockito::Server::new_async().await;
    let _config = server
        .mock("GET", "/v1/config")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .create_async()
        .await;
    let metadata = serde_json::to_value(fixture.metadata()).unwrap();
    let body = serde_json::json!({
        "metadata-location": fixture.metadata_location(),
        "metadata": metadata,
        "config": {}
    });
    let _table = server
        .mock("GET", "/v1/namespaces/db/tables/events")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body.to_string())
        .create_async()
        .await;

    let mut conn = pool.acquire().await.unwrap();
    let organization = OrganizationId::generate();
    tenancy::create_organization(
        &mut conn,
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
        &mut conn,
        &NewCatalogConnection {
            id: connection,
            organization_id: organization,
            name: "main".to_owned(),
            uri: server.url(),
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
    let table_id = record_observation(&mut conn, organization, connection, &observed)
        .await
        .unwrap();
    let created_by = PrincipalId::generate();
    tenancy::create_principal(
        &mut conn,
        &NewPrincipal {
            id: created_by,
            organization_id: organization,
            kind: PrincipalKind::User,
            issuer: "https://idp.example".to_owned(),
            subject: format!("planner-{created_by}"),
            display_name: "Planner".to_owned(),
        },
    )
    .await
    .unwrap();
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: organization,
            principal_id: created_by,
            role: Role::Administrator,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    let job = jobs::create_job(
        &mut conn,
        &NewJob {
            id: JobId::generate(),
            organization_id: organization,
            table_id,
            retry_of: None,
            created_by,
        },
    )
    .await
    .unwrap();
    let epoch = runtime_epoch(&mut conn).await;
    drop(conn);

    let mut settings =
        DatabaseConfig::new(bergsmith_persistence::DatabaseUrl::new(database_url(&pool)));
    settings.max_connections = 1;
    let database = Database::connect(&settings).await.unwrap();
    let profiles = RuntimeProfiles {
        default: StorageProfiles::new([StorageProfile::LocalFs]),
        connections: std::collections::BTreeMap::new(),
    };
    let catalogs = Arc::new(RuntimeCatalogs::new(database, profiles));
    let access = Arc::new(RuntimeWorkerAccess::new(catalogs.clone()));
    let worker = WorkerService::new(
        pool.clone(),
        epoch,
        Vec::new(),
        WorkerConfig::default(),
        access.clone(),
    )
    .unwrap();
    let scheduler = Arc::new(Scheduler::new(worker, access, SchedulerConfig::default()).unwrap());
    let planner = PlannerService::new(
        pool.clone(),
        catalogs,
        scheduler,
        PlannerConfig {
            min_input_files: 2,
            min_files_removed: 1,
            task_overhead_millis: 1,
            ..PlannerConfig::default()
        },
        TaskSettings {
            pool: "default".into(),
            capability: "native-v1".into(),
            memory_bytes: 1_073_741_824,
            cpu_millis: 1_000,
            max_duration_seconds: 180,
            max_output_bytes: 4_294_967_296,
        },
        64 * 1024 * 1024 * 1024,
        RetryPolicy::default(),
    )
    .unwrap();

    let summary = planner.pass().await.unwrap();
    assert_eq!(summary.planned, 1, "one plan");
    assert_eq!(summary.registered, 1, "one dispatch registration");

    let current = jobs::job(&mut pool.acquire().await.unwrap(), organization, job.id)
        .await
        .unwrap();
    assert_eq!(current.state, JobState::Queued);
    let specs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dispatch_specs d JOIN tasks t ON t.id=d.task_id WHERE t.job_id=$1",
    )
    .bind(job.id.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(specs, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_scheduled_job_is_planned_with_its_policy_budget(pool: PgPool) {
    let fixture = fixture().await;
    let mut server = mockito::Server::new_async().await;
    server
        .mock("GET", "/v1/config")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .create_async()
        .await;
    let metadata = serde_json::to_value(fixture.metadata()).unwrap();
    server
        .mock("GET", "/v1/namespaces/db/tables/events")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            serde_json::json!({
                "metadata-location": fixture.metadata_location(),
                "metadata": metadata,
                "config": {}
            })
            .to_string(),
        )
        .create_async()
        .await;

    let mut conn = pool.acquire().await.unwrap();
    let organization = OrganizationId::generate();
    tenancy::create_organization(
        &mut conn,
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
        &mut conn,
        &NewCatalogConnection {
            id: connection,
            organization_id: organization,
            name: "main".to_owned(),
            uri: server.url(),
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
    let table_id = record_observation(&mut conn, organization, connection, &observed)
        .await
        .unwrap();
    let service = PrincipalId::generate();
    tenancy::create_principal(
        &mut conn,
        &NewPrincipal {
            id: service,
            organization_id: organization,
            kind: PrincipalKind::Service,
            issuer: "bergsmith:service".to_owned(),
            subject: service.to_string(),
            display_name: "scheduler".to_owned(),
        },
    )
    .await
    .unwrap();
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: organization,
            principal_id: service,
            role: Role::MaintenanceService,
            scope: GrantScope::table(table_id),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    let job = jobs::create_job(
        &mut conn,
        &NewJob {
            id: JobId::generate(),
            organization_id: organization,
            table_id,
            retry_of: None,
            created_by: service,
        },
    )
    .await
    .unwrap();
    let policy = bergsmith_common::ids::PolicyId::generate();
    sqlx::query(
        "INSERT INTO maintenance_policies
             (id, organization_id, name, operation, target_file_size_bytes,
              min_input_files, max_input_bytes, created_by)
         VALUES ($1, $2, 'small', 'Compact', $3, $4, $5, $6)",
    )
    .bind(policy.as_uuid())
    .bind(organization.as_uuid())
    .bind(512_i64 * 1024 * 1024)
    .bind(3_i32)
    .bind(3_i64 * 1024 * 1024 * 1024)
    .bind(service.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let schedule = bergsmith_common::ids::ScheduleId::generate();
    sqlx::query(
        "INSERT INTO maintenance_schedules
             (id, organization_id, policy_id, table_identity_id, cron_expression,
              time_zone, service_principal_id, created_by)
         VALUES ($1, $2, $3, $4, '0 3 * * *', 'UTC', $5, $5)",
    )
    .bind(schedule.as_uuid())
    .bind(organization.as_uuid())
    .bind(policy.as_uuid())
    .bind(table_id.as_uuid())
    .bind(service.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO maintenance_schedule_occurrences
             (id, organization_id, schedule_id, scheduled_for, state, job_id)
         VALUES ($1, $2, $3, now(), 'Admitted', $4)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(organization.as_uuid())
    .bind(schedule.as_uuid())
    .bind(job.id.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let epoch = runtime_epoch(&mut conn).await;
    drop(conn);

    let mut settings =
        DatabaseConfig::new(bergsmith_persistence::DatabaseUrl::new(database_url(&pool)));
    settings.max_connections = 1;
    let database = Database::connect(&settings).await.unwrap();
    let profiles = RuntimeProfiles {
        default: StorageProfiles::new([StorageProfile::LocalFs]),
        connections: std::collections::BTreeMap::new(),
    };
    let catalogs = Arc::new(RuntimeCatalogs::new(database, profiles));
    let access = Arc::new(RuntimeWorkerAccess::new(catalogs.clone()));
    let worker = WorkerService::new(
        pool.clone(),
        epoch,
        Vec::new(),
        WorkerConfig::default(),
        access.clone(),
    )
    .unwrap();
    let scheduler = Arc::new(Scheduler::new(worker, access, SchedulerConfig::default()).unwrap());
    let planner = PlannerService::new(
        pool.clone(),
        catalogs,
        scheduler,
        PlannerConfig {
            min_input_files: 5,
            min_files_removed: 2,
            task_overhead_millis: 1,
            ..PlannerConfig::default()
        },
        TaskSettings {
            pool: "default".into(),
            capability: "native-v1".into(),
            memory_bytes: 1_073_741_824,
            cpu_millis: 1_000,
            max_duration_seconds: 180,
            max_output_bytes: 4_294_967_296,
        },
        1024 * 1024 * 1024,
        RetryPolicy::default(),
    )
    .unwrap();
    let summary = planner.pass().await.unwrap();
    assert_eq!(summary.planned, 1);

    let mut conn = pool.acquire().await.unwrap();
    let stored = bergsmith_coordinator::planning::load_plan(&mut conn, job.id)
        .await
        .unwrap()
        .unwrap();
    let config = stored.config();
    assert_eq!(config.target_file_size_bytes, 512 * 1024 * 1024);
    assert_eq!(config.min_input_files, 3);
    assert_eq!(config.max_job_input_bytes, 1024 * 1024 * 1024);
    assert_eq!(config.max_group_input_bytes, 1024 * 1024 * 1024);
}
