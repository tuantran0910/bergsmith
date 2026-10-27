//! BS-031.T2: orphan-file cleanup planning and bounded deletion (ADR 0062).
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bergsmith_common::ids::JobId;
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_coordinator::durable_publication::PublicationConfig;
use bergsmith_coordinator::runtime::PublicationService;
use bergsmith_coordinator::runtime::catalogs::{RuntimeCatalogs, RuntimeWorkerAccess};
use bergsmith_coordinator::runtime::config::{OrphanConfig, RuntimeProfiles, TaskSettings};
use bergsmith_coordinator::runtime::planning::PlannerService;
use bergsmith_coordinator::scheduling::{Scheduler, SchedulerConfig};
use bergsmith_coordinator::workers::{WorkerConfig, WorkerService};
use bergsmith_fixtures::{Commit, FileSpec, Fixture, TableSpec, row};
use bergsmith_maintenance::PlannerConfig;
use bergsmith_maintenance::orphans::OrphanPlanDocument;
use bergsmith_persistence::scheduling::RetryPolicy;
use bergsmith_persistence::{
    Database, DatabaseConfig, DatabaseUrl, jobs, leadership::acquire_leadership, orphans, tables,
};
use bergsmith_storage::{Location, StorageProfile, StorageProfiles};
use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
use mockito::{Server, ServerGuard};
use serde_json::json;
use sqlx::{ConnectOptions, PgPool};

const TABLE_PATH: &str = "/v1/namespaces/db/tables/t";

struct Run {
    fixture: Fixture,
    tenant: support::Tenant,
    job: jobs::Job,
    server: ServerGuard,
    orphan_data: PathBuf,
    orphan_metadata: PathBuf,
    bergsmith_output: PathBuf,
    planner: PlannerService,
    publication: PublicationService,
}

async fn fixture() -> Fixture {
    let schema = Schema::builder()
        .with_fields([
            Arc::new(NestedField::required(
                1,
                "id",
                Type::Primitive(PrimitiveType::Long),
            )),
            Arc::new(NestedField::optional(
                2,
                "name",
                Type::Primitive(PrimitiveType::String),
            )),
        ])
        .build()
        .unwrap();
    TableSpec::new(schema)
        .commit(
            Commit::new()
                .file(FileSpec::data("a", [row![1 => 1_i64, 2 => "a"]]))
                .file(FileSpec::data("b", [row![1 => 2_i64, 2 => "b"]])),
        )
        .generate()
        .await
        .unwrap()
}

fn enabled() -> OrphanConfig {
    OrphanConfig {
        enabled: true,
        // The test writes its orphans just before the pass, so any positive
        // minimum age would keep them.
        minimum_age_seconds: 0,
        max_listed_objects: 10_000,
        max_scan_entries: 100_000,
        max_candidates: 100,
        max_deletions_per_pass: 50,
    }
}

fn task_settings() -> TaskSettings {
    TaskSettings {
        pool: "default".into(),
        capability: "native-v1".into(),
        memory_bytes: 1_073_741_824,
        cpu_millis: 1_000,
        max_duration_seconds: 180,
        max_output_bytes: 4_294_967_296,
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one fixture keeps the table, catalog mock, and both services explicit"
)]
async fn setup(pool: &PgPool, orphan: OrphanConfig) -> Run {
    let fixture = fixture().await;
    let root = fixture.root().to_owned();
    let orphan_data = root.join("data/orphan-left-behind.parquet");
    let orphan_metadata = root.join("metadata/orphan-left-behind.metadata.json");
    let bergsmith_output = root.join("bergsmith/job/attempt/output.parquet");
    std::fs::create_dir_all(bergsmith_output.parent().unwrap()).unwrap();
    // Older than the connection's 180s execution plus 3600s unresolved
    // publication window, so age never hides the orphans.
    let two_days_ago = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
    for (path, bytes) in [
        (&orphan_data, b"orphan".as_slice()),
        (&orphan_metadata, b"{}".as_slice()),
        (&bergsmith_output, b"output".as_slice()),
    ] {
        std::fs::write(path, bytes).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(two_days_ago)
            .unwrap();
    }

    let mut conn = pool.acquire().await.unwrap();
    let mut tenant = support::tenant(&mut conn, "acme").await;
    tenant.table = tables::resolve_table(
        &mut conn,
        tenant.organization,
        &tables::TableAlias {
            connection_id: tenant.connection,
            namespace: vec!["db".into()],
            name: "t".into(),
        },
        fixture.metadata().uuid(),
    )
    .await
    .unwrap()
    .id;
    let job = jobs::create_job(
        &mut conn,
        &jobs::NewJob {
            id: JobId::generate(),
            organization_id: tenant.organization,
            table_id: tenant.table,
            operation: JobOperation::RemoveOrphanFiles,
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

    let mut server = Server::new_async().await;
    sqlx::query(
        "UPDATE catalog_connections SET uri = $2, credential_ref = NULL, warehouse = NULL
         WHERE organization_id = $1 AND id = $3",
    )
    .bind(tenant.organization.as_uuid())
    .bind(server.url())
    .bind(tenant.connection.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    server
        .mock("GET", "/v1/config")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .expect_at_least(1)
        .create_async()
        .await;
    server
        .mock("GET", TABLE_PATH)
        .with_body(
            json!({
                "metadata-location": fixture.metadata_location(),
                "metadata": fixture.metadata(),
            })
            .to_string(),
        )
        .expect_at_least(2)
        .create_async()
        .await;

    let mut config = DatabaseConfig::new(DatabaseUrl::new(
        pool.connect_options().to_url_lossy().to_string(),
    ));
    config.max_connections = 1;
    let database = Database::connect(&config).await.unwrap();
    let catalogs = Arc::new(RuntimeCatalogs::new(
        database,
        RuntimeProfiles {
            default: StorageProfiles::new([StorageProfile::LocalFs]),
            connections: BTreeMap::new(),
        },
    ));
    let access = Arc::new(RuntimeWorkerAccess::new(catalogs.clone()));
    let worker =
        WorkerService::new(pool.clone(), epoch, WorkerConfig::default(), access.clone()).unwrap();
    let scheduler = Arc::new(Scheduler::new(worker, access, SchedulerConfig::default()).unwrap());
    let planner = PlannerService::new(
        pool.clone(),
        catalogs.clone(),
        scheduler,
        PlannerConfig::default(),
        task_settings(),
        64 * 1024 * 1024 * 1024,
        RetryPolicy::default(),
        false,
        orphan,
    )
    .unwrap();
    let publication = PublicationService::new(
        pool.clone(),
        catalogs,
        epoch,
        PublicationConfig::default(),
        RetryPolicy::default(),
        false,
        orphan,
    );
    Run {
        fixture,
        tenant,
        job,
        server,
        orphan_data,
        orphan_metadata,
        bergsmith_output,
        planner,
        publication,
    }
}

fn file_uri(path: &std::path::Path) -> String {
    format!("file://{}", path.display())
}

fn local_path(uri: &str) -> PathBuf {
    PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri))
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_cleanup_plan_deletes_only_the_unreferenced_objects(pool: PgPool) {
    let mut run = setup(&pool, enabled()).await;
    let mut conn = pool.acquire().await.unwrap();
    let planned = run.planner.pass().await.unwrap();
    let job = jobs::job(&mut conn, run.tenant.organization, run.job.id)
        .await
        .unwrap();
    assert_eq!(
        planned.planned, 1,
        "one orphan plan: {planned:?} state={:?} reason={:?}",
        job.state, job.state_reason
    );
    assert_eq!(job.state, JobState::ReadyToCommit);
    let (content, _hash) = orphans::orphan_plan(&mut conn, job.id)
        .await
        .unwrap()
        .expect("an orphan plan is recorded");
    let document = OrphanPlanDocument::from_persisted(content).unwrap();
    let mut candidates: Vec<String> = document
        .candidates
        .iter()
        .map(|candidate| candidate.path.clone())
        .collect();
    candidates.sort();
    let mut expected = vec![
        Location::parse(&file_uri(&run.orphan_data))
            .unwrap()
            .to_string(),
        Location::parse(&file_uri(&run.orphan_metadata))
            .unwrap()
            .to_string(),
    ];
    expected.sort();
    assert_eq!(candidates, expected);
    assert!(
        !candidates.iter().any(|path| path.contains("/bergsmith/")),
        "Bergsmith's own tree is never a candidate"
    );

    let cleaned = run.publication.pass().await.unwrap();
    assert_eq!(cleaned.cleaned, 1, "one deletion pass");
    let job = jobs::job(&mut conn, run.tenant.organization, run.job.id)
        .await
        .unwrap();
    assert_eq!(job.state, JobState::Succeeded, "{:?}", job.state_reason);
    assert!(!run.orphan_data.exists(), "the orphan data file is gone");
    assert!(
        !run.orphan_metadata.exists(),
        "the orphan metadata file is gone"
    );
    assert!(
        run.bergsmith_output.exists(),
        "worker outputs are never removed"
    );
    let live = local_path(run.fixture.path_of("a").unwrap());
    assert!(live.exists(), "referenced data survives");
    assert!(
        local_path(run.fixture.metadata_location()).exists(),
        "the current metadata file survives"
    );
    assert_eq!(
        orphans::receipt_totals(&mut conn, job.id).await.unwrap().0,
        2
    );
    run.server.reset();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cleanup_is_refused_when_disabled(pool: PgPool) {
    let mut run = setup(&pool, OrphanConfig::default()).await;
    let mut conn = pool.acquire().await.unwrap();
    let summary = run.planner.pass().await.unwrap();
    assert_eq!(summary.failed, 1);
    let job = jobs::job(&mut conn, run.tenant.organization, run.job.id)
        .await
        .unwrap();
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(
        job.state_reason.as_deref(),
        Some("OrphanCleanupUnavailable")
    );
    assert!(
        run.orphan_data.exists(),
        "a refused cleanup deletes nothing"
    );
    run.server.reset();
}
