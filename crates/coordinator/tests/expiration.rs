//! BS-031.T1: metadata-only snapshot expiration publication.
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "acceptance fixtures fail on setup errors"
)]

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

use std::time::Duration;

use bergsmith_catalog::{Baseline, CatalogConnection, ConnectionConfig, LoadedTable, TableName};
use bergsmith_common::ids::{JobId, PlanId};
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_coordinator::durable_publication::{self as publication, DurablePublicationError};
use bergsmith_coordinator::runtime::PublicationService;
use bergsmith_coordinator::runtime::catalogs::RuntimeCatalogs;
use bergsmith_coordinator::runtime::config::RuntimeProfiles;
use bergsmith_fixtures::Fixture;
use bergsmith_maintenance::{
    ExpirationPlanDocument, ExpirationPolicy, PlanningBaseline, plan_expiration,
};
use bergsmith_persistence::leadership::acquire_leadership;
use bergsmith_persistence::scheduling::RetryPolicy;
use bergsmith_persistence::{
    Database, DatabaseConfig, DatabaseUrl, expirations, jobs, publications,
};
use bergsmith_storage::{Location, StorageProfile, StorageProfiles};
use chrono::Utc;
use iceberg::spec::TableMetadata;
use mockito::{Matcher, Server, ServerGuard};
use serde_json::json;
use sqlx::{ConnectOptions, PgPool};

const TABLE_PATH: &str = "/v1/namespaces/db/tables/t";

struct Run {
    fixture: Fixture,
    tenant: support::Tenant,
    job: jobs::Job,
    epoch: i64,
    server: ServerGuard,
    catalog: CatalogConnection,
    baseline: Baseline,
    load: mockito::Mock,
    plan: ExpirationPlanDocument,
}

async fn load_mock(
    server: &mut Server,
    fixture: &Fixture,
    metadata: &TableMetadata,
) -> mockito::Mock {
    server
        .mock("GET", TABLE_PATH)
        .with_body(
            json!({"metadata-location": fixture.metadata_location(), "metadata": metadata})
                .to_string(),
        )
        .create_async()
        .await
}

fn loaded(fixture: &Fixture) -> LoadedTable {
    LoadedTable {
        name: TableName {
            namespace: vec!["db".into()],
            name: "t".into(),
        },
        metadata_location: Some(fixture.metadata_location().into()),
        metadata: fixture.metadata().clone(),
        location: Location::parse(fixture.metadata().location()).unwrap(),
        credentials: vec![],
    }
}

async fn setup(pool: &PgPool) -> Run {
    let fixture = bergsmith_fixtures::spike::equality_deletes_with_old_and_same_sequence_data()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let mut tenant = support::tenant(&mut conn, "acme").await;
    tenant.table = bergsmith_persistence::tables::resolve_table(
        &mut conn,
        tenant.organization,
        &bergsmith_persistence::tables::TableAlias {
            connection_id: tenant.connection,
            namespace: vec!["db".into()],
            name: "t".into(),
        },
        fixture.metadata().uuid(),
    )
    .await
    .unwrap()
    .id;
    let job_id = JobId::generate();
    let job = jobs::create_job(
        &mut conn,
        &jobs::NewJob {
            id: job_id,
            organization_id: tenant.organization,
            table_id: tenant.table,
            operation: JobOperation::ExpireSnapshots,
            retry_of: None,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();
    let epoch = acquire_leadership(&mut conn, "test", Duration::from_secs(300))
        .await
        .unwrap()
        .expect("leadership is acquired")
        .epoch;
    let policy = ExpirationPolicy::from_metadata(fixture.metadata(), 1, 0, []).unwrap();
    let decision = plan_expiration(fixture.metadata(), &policy, Utc::now()).unwrap();
    assert!(
        !decision.remove.is_empty(),
        "the older fixture snapshot expires"
    );
    let baseline = PlanningBaseline::capture(fixture.metadata(), Some(fixture.metadata_location()))
        .unwrap()
        .unwrap();
    let plan =
        ExpirationPlanDocument::new(job.id, PlanId::generate(), baseline, &decision, &policy)
            .unwrap();
    let content = plan.to_value().unwrap();
    let hash = plan.content_hash().unwrap().to_hex();
    let job = expirations::create_expiration_plan(
        &mut conn,
        &expirations::NewExpirationPlan {
            id: plan.plan_id,
            job_id: job.id,
            content: &content,
            content_hash: &hash,
        },
        job.state_revision,
        epoch,
    )
    .await
    .unwrap();
    drop(conn);

    let mut server = Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .create_async()
        .await;
    let catalog = CatalogConnection::connect(&ConnectionConfig::new(
        url::Url::parse(&server.url()).unwrap(),
    ))
    .await
    .unwrap();
    config.assert_async().await;
    let load = load_mock(&mut server, &fixture, fixture.metadata()).await;
    let baseline = Baseline::new(&loaded(&fixture), fixture.file_io().clone()).unwrap();
    Run {
        fixture,
        tenant,
        job,
        epoch,
        server,
        catalog,
        baseline,
        load,
        plan,
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn expiration_prepares_sends_and_records_success(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&pool).await;
    let job = publication::prepare_expiration(
        &mut conn,
        &run.job,
        run.epoch,
        run.tenant.connection,
        &run.baseline,
        &run.plan,
    )
    .await
    .unwrap();
    assert_eq!(job.state, JobState::Committing);
    let intent = publications::intent(&mut conn, job.id).await.unwrap();
    assert_eq!(intent.content["remove"], json!(run.plan.remove));
    assert_eq!(intent.content["protected"], json!([]));
    let commit = run
        .server
        .mock("POST", TABLE_PATH)
        .match_header("idempotency-key", Matcher::Missing)
        .with_status(200)
        .with_body("{}")
        .expect(1)
        .create_async()
        .await;
    let done = publication::publish_expiration(&mut conn, &job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(done.state, JobState::Succeeded);
    let barriers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM publication_barriers WHERE job_id = $1")
            .bind(job.id.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(barriers, 0);
    commit.assert_async().await;
    run.load.remove_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_ambiguous_expiration_is_resolved_only_by_the_expected_state(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&pool).await;
    let job = publication::prepare_expiration(
        &mut conn,
        &run.job,
        run.epoch,
        run.tenant.connection,
        &run.baseline,
        &run.plan,
    )
    .await
    .unwrap();
    let commit = run
        .server
        .mock("POST", TABLE_PATH)
        .with_status(503)
        .with_body("{}")
        .expect(1)
        .create_async()
        .await;
    let unknown = publication::publish_expiration(&mut conn, &job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(unknown.state, JobState::CommitOutcomeUnknown);
    commit.assert_async().await;

    // The table still has the removed snapshot: absence is not proven.
    let still = publication::reconcile(&mut conn, &unknown, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(still.state, JobState::CommitOutcomeUnknown);

    // The catalog now shows the exact post-state of the intent.
    let request = publications::intent(&mut conn, job.id).await.unwrap();
    let request = bergsmith_catalog::CommitRequest::from_persisted(
        TableName {
            namespace: vec!["db".into()],
            name: "t".into(),
        },
        request.request_body,
    );
    let mut builder = run.fixture.metadata().clone().into_builder(None);
    for update in request.decode().unwrap().updates {
        builder = update.apply(builder).unwrap();
    }
    let published = builder.build().unwrap().metadata;
    run.load.remove_async().await;
    run.load = load_mock(&mut run.server, &run.fixture, &published).await;
    let resolved = publication::reconcile(&mut conn, &unknown, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(resolved.state, JobState::Succeeded);
    run.load.remove_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_snapshot_pinned_after_planning_replans(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let run = setup(&pool).await;
    let pinned = run.plan.remove[0];
    let other = support::job(&mut conn, &run.tenant).await;
    // Simulate a plan persisted by an older release before plan creation
    // rejected expiring baselines; preparation must still recheck it.
    sqlx::query(
        "INSERT INTO plans (id, job_id, content, content_hash)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(PlanId::generate().as_uuid())
    .bind(other.as_uuid())
    .bind(json!({ "baseline": { "snapshot_id": pinned.to_string() } }))
    .bind(support::digest('a'))
    .execute(&mut *conn)
    .await
    .unwrap();
    let error = publication::prepare_expiration(
        &mut conn,
        &run.job,
        run.epoch,
        run.tenant.connection,
        &run.baseline,
        &run.plan,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        DurablePublicationError::ProtectedSnapshot(_)
    ));
    run.load.remove_async().await;
}

async fn publication_service(pool: &PgPool, epoch: i64, safety: bool) -> PublicationService {
    let mut config = DatabaseConfig::new(DatabaseUrl::new(
        pool.connect_options().to_url_lossy().to_string(),
    ));
    config.max_connections = 1;
    let database = Database::connect(&config).await.unwrap();
    let catalogs = std::sync::Arc::new(RuntimeCatalogs::new(
        database,
        RuntimeProfiles {
            default: StorageProfiles::new([StorageProfile::LocalFs]),
            connections: std::collections::BTreeMap::new(),
        },
    ));
    PublicationService::new(
        pool.clone(),
        catalogs,
        epoch,
        publication::PublicationConfig::default(),
        RetryPolicy::default(),
        safety,
    )
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_restart_without_reference_safety_fails_a_prepared_expiration(pool: PgPool) {
    let run = setup(&pool).await;
    let service = publication_service(&pool, run.epoch, false).await;
    let summary = service.pass().await.unwrap();
    assert_eq!(summary.failed, 1);
    let mut conn = pool.acquire().await.unwrap();
    let current = jobs::job(&mut conn, run.tenant.organization, run.job.id)
        .await
        .unwrap();
    assert_eq!(current.state, JobState::Failed);
    assert_eq!(
        current.state_reason.as_deref(),
        Some("ExpirationRefSafetyUnavailable")
    );
    run.load.remove_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_restart_without_reference_safety_fails_an_unsent_intent(pool: PgPool) {
    let run = setup(&pool).await;
    let job = publication::prepare_expiration(
        &mut pool.acquire().await.unwrap(),
        &run.job,
        run.epoch,
        run.tenant.connection,
        &run.baseline,
        &run.plan,
    )
    .await
    .unwrap();
    assert_eq!(job.state, JobState::Committing);
    let service = publication_service(&pool, run.epoch, false).await;
    let summary = service.pass().await.unwrap();
    assert_eq!(summary.failed, 1);
    let mut conn = pool.acquire().await.unwrap();
    let current = jobs::job(&mut conn, run.tenant.organization, job.id)
        .await
        .unwrap();
    assert_eq!(current.state, JobState::Failed);
    assert_eq!(
        current.state_reason.as_deref(),
        Some("ExpirationRefSafetyUnavailable")
    );
    let barriers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM publication_barriers WHERE job_id = $1")
            .bind(job.id.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(barriers, 0);
    run.load.remove_async().await;
}
