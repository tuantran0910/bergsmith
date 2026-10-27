//! Recording planning outcomes in the ledger (BS-009.T6).

// clippy's `allow-*-in-tests` settings cover only `#[test]` functions; the
// fixtures below fail the test on setup errors.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test fixtures fail the test on setup errors"
)]

use std::sync::Arc;

/// The execution budget the tests plan with.
const EXECUTION_BUDGET: std::time::Duration = std::time::Duration::from_secs(180);

use bergsmith_catalog::{ObservedTable, TableName};
use bergsmith_common::ids::{
    CatalogConnectionId, JobId, OrganizationId, PlanId, PrincipalId, RoleGrantId,
};
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_common::task::TaskState;
use bergsmith_coordinator::inventory::record_observation;
use bergsmith_coordinator::planning::{
    PlanningError, UNSUPPORTED_TABLE_REASON, load_plan, record_outcome,
};
use bergsmith_fixtures::{Commit, FileSpec, Fixture, TableSpec, row};
use bergsmith_maintenance::{
    NoOpReason, PlanDocument, PlanIds, PlanInput, PlanOutcome, Planner, PlannerConfig, Rejection,
};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::jobs::{self, Job, NewJob};
use bergsmith_persistence::plans;
use bergsmith_persistence::tenancy::{
    self, NewCatalogConnection, NewOrganization, NewPrincipal, PrincipalKind,
};
use bergsmith_storage::Location;
use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
use sqlx::{PgConnection, PgPool};

/// Five small files, so the default configuration plans one task.
async fn table() -> Fixture {
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

/// A job in `Planning` for `fixture`'s table.
async fn job(conn: &mut PgConnection, fixture: &Fixture) -> Job {
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
            uri: "http://127.0.0.1:58181/catalog".to_owned(),
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
    let table_id = record_observation(conn, organization, connection, &observed)
        .await
        .unwrap();
    // A requester is mandatory; grants let the fixture reach admission.
    let created_by = PrincipalId::generate();
    tenancy::create_principal(
        conn,
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
        conn,
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
    jobs::create_job(
        conn,
        &NewJob {
            id: JobId::generate(),
            organization_id: organization,
            table_id,
            operation: JobOperation::Compact,
            retry_of: None,
            created_by,
        },
    )
    .await
    .unwrap()
}

async fn plan(fixture: &Fixture, job_id: JobId) -> PlanOutcome {
    Planner::new(PlannerConfig::default())
        .unwrap()
        .plan(
            PlanInput {
                metadata: fixture.metadata(),
                metadata_location: Some(fixture.metadata_location()),
                file_io: fixture.file_io(),
                selection: &[],
            },
            PlanIds {
                job_id,
                plan_id: PlanId::generate(),
            },
        )
        .await
        .unwrap()
}

fn document(outcome: &PlanOutcome) -> &PlanDocument {
    match outcome {
        PlanOutcome::Planned(document) => document,
        other => panic!("expected a plan, got {other:?}"),
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_plan_and_the_queued_job_commit_together(pool: PgPool) {
    let fixture = table().await;
    let mut conn = pool.acquire().await.unwrap();
    let job = job(&mut conn, &fixture).await;
    let outcome = plan(&fixture, job.id).await;

    let queued = record_outcome(&mut conn, &job, &outcome, EXECUTION_BUDGET)
        .await
        .unwrap();

    assert_eq!(queued.state, JobState::Queued);
    assert_eq!(queued.state_revision, job.state_revision + 1);
    let stored = load_plan(&mut conn, job.id).await.unwrap().unwrap();
    assert_eq!(&stored, document(&outcome));
    let row = plans::plan_for_job(&mut conn, job.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.content_hash, stored.sha256().unwrap().to_hex());
    let tasks = plans::tasks(&mut conn, row.id).await.unwrap();
    assert_eq!(
        tasks.iter().map(|task| task.id).collect::<Vec<_>>(),
        stored.task_ids()
    );
    assert!(tasks.iter().all(|task| task.state == TaskState::Pending));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_stale_revision_records_nothing(pool: PgPool) {
    let fixture = table().await;
    let mut conn = pool.acquire().await.unwrap();
    let job = job(&mut conn, &fixture).await;
    let outcome = plan(&fixture, job.id).await;
    let stale = Job {
        state_revision: job.state_revision + 5,
        ..job.clone()
    };

    let error = record_outcome(&mut conn, &stale, &outcome, EXECUTION_BUDGET)
        .await
        .unwrap_err();

    assert!(
        matches!(
            error,
            PlanningError::Persistence(PersistenceError::RevisionConflict { .. })
        ),
        "{error}"
    );
    assert!(load_plan(&mut conn, job.id).await.unwrap().is_none());
    assert_eq!(
        jobs::job(&mut conn, job.organization_id, job.id)
            .await
            .unwrap()
            .state,
        JobState::Planning
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_job_is_planned_once(pool: PgPool) {
    let fixture = table().await;
    let mut conn = pool.acquire().await.unwrap();
    let job = job(&mut conn, &fixture).await;
    let first = plan(&fixture, job.id).await;
    let queued = record_outcome(&mut conn, &job, &first, EXECUTION_BUDGET)
        .await
        .unwrap();

    let error = record_outcome(
        &mut conn,
        &queued,
        &plan(&fixture, job.id).await,
        EXECUTION_BUDGET,
    )
    .await
    .unwrap_err();

    assert!(
        matches!(
            error,
            PlanningError::Persistence(PersistenceError::JobNotInState { .. })
        ),
        "{error}"
    );
    assert_eq!(
        load_plan(&mut conn, job.id).await.unwrap().as_ref(),
        Some(document(&first))
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_plan_for_another_job_is_refused(pool: PgPool) {
    let fixture = table().await;
    let mut conn = pool.acquire().await.unwrap();
    let job = job(&mut conn, &fixture).await;
    let other = JobId::generate();

    let error = record_outcome(
        &mut conn,
        &job,
        &plan(&fixture, other).await,
        EXECUTION_BUDGET,
    )
    .await
    .unwrap_err();

    assert!(
        matches!(error, PlanningError::JobMismatch { job: j, plan_job } if j == job.id && plan_job == other),
        "{error}"
    );
    assert!(load_plan(&mut conn, job.id).await.unwrap().is_none());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn no_work_and_rejections_end_the_job(pool: PgPool) {
    let fixture = table().await;
    let mut conn = pool.acquire().await.unwrap();
    let noop_job = job(&mut conn, &fixture).await;
    let failed_job = job(&mut conn, &fixture).await;

    let noop = record_outcome(
        &mut conn,
        &noop_job,
        &PlanOutcome::NoOp(NoOpReason::NoBeneficialWork),
        EXECUTION_BUDGET,
    )
    .await
    .unwrap();
    let failed = record_outcome(
        &mut conn,
        &failed_job,
        &PlanOutcome::Rejected(vec![Rejection::Encrypted]),
        EXECUTION_BUDGET,
    )
    .await
    .unwrap();

    assert_eq!(
        (noop.state, noop.state_reason.as_deref()),
        (JobState::NoOp, Some("no_beneficial_work"))
    );
    assert_eq!(
        (failed.state, failed.state_reason.as_deref()),
        (JobState::Failed, Some(UNSUPPORTED_TABLE_REASON))
    );
    assert!(load_plan(&mut conn, noop_job.id).await.unwrap().is_none());
}
