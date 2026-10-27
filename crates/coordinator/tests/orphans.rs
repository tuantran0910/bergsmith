//! BS-031.T2: orphan-file cleanup planning and bounded deletion (ADR 0062).
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bergsmith_common::ids::{JobId, PlanId};
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
use bergsmith_persistence::cancellation::{self, CancelJob, CancellationDisposition};
use bergsmith_persistence::jobs::NewJob;
use bergsmith_persistence::orphans::{self, Disposition};
use bergsmith_persistence::scheduling::RetryPolicy;
use bergsmith_persistence::{
    Database, DatabaseConfig, DatabaseUrl, jobs, leadership::acquire_leadership, tables,
};
use bergsmith_storage::{Location, StorageProfile, StorageProfiles};
use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
use mockito::{Server, ServerGuard};
use serde_json::json;
use sqlx::{ConnectOptions, PgPool};
use support::{digest, force_job_state};

const TABLE_PATH: &str = "/v1/namespaces/db/tables/t";

struct Run {
    pool: PgPool,
    fixture: Fixture,
    tenant: support::Tenant,
    job: jobs::Job,
    epoch: i64,
    catalogs: Arc<RuntimeCatalogs>,
    server: ServerGuard,
    orphan_data: PathBuf,
    orphan_metadata: PathBuf,
    bergsmith_outputs: Vec<PathBuf>,
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

fn enabled(pass: usize) -> OrphanConfig {
    OrphanConfig {
        enabled: true,
        // The test writes its orphans just before the pass, so any positive
        // minimum age would keep them.
        minimum_age_seconds: 0,
        max_listed_objects: 10_000,
        max_scan_entries: 100_000,
        max_scan_bytes: 64 * 1024 * 1024,
        max_candidates: 100,
        max_deletions_per_pass: pass,
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
async fn setup(pool: &PgPool) -> Run {
    let fixture = fixture().await;
    let root = fixture.root().to_owned();
    let orphan_data = root.join("data/orphan-left-behind.parquet");
    let orphan_metadata = root.join("metadata/orphan-left-behind.metadata.json");
    // Bergsmith writes under the data location, not the table root, and an
    // older layout used the table root. Both must survive.
    let data_output = root.join("data/bergsmith/job/attempt/output.parquet");
    let legacy_output = root.join("bergsmith/job/attempt/output.parquet");
    for path in [&data_output, &legacy_output] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    }
    // Older than the connection's 180s execution plus 3600s unresolved
    // publication window, so age never hides the orphans.
    let two_days_ago = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
    for (path, bytes) in [
        (&orphan_data, b"orphan".as_slice()),
        (&orphan_metadata, b"{}".as_slice()),
        (&data_output, b"output".as_slice()),
        (&legacy_output, b"output".as_slice()),
    ] {
        write_older(path, bytes, two_days_ago);
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
        &NewJob {
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
    Run {
        pool: pool.clone(),
        fixture,
        tenant,
        job,
        epoch,
        catalogs,
        server,
        orphan_data,
        orphan_metadata,
        bergsmith_outputs: vec![data_output, legacy_output],
    }
}

fn publication(run: &Run, orphan: OrphanConfig) -> PublicationService {
    PublicationService::new(
        run.pool.clone(),
        run.catalogs.clone(),
        run.epoch,
        PublicationConfig::default(),
        RetryPolicy::default(),
        false,
        orphan,
    )
}

fn planner(run: &Run, orphan: OrphanConfig) -> PlannerService {
    let access = Arc::new(RuntimeWorkerAccess::new(run.catalogs.clone()));
    let worker = WorkerService::new(
        run.pool.clone(),
        run.epoch,
        WorkerConfig::default(),
        access.clone(),
    )
    .unwrap();
    let scheduler = Arc::new(Scheduler::new(worker, access, SchedulerConfig::default()).unwrap());
    PlannerService::new(
        run.pool.clone(),
        run.catalogs.clone(),
        scheduler,
        PlannerConfig::default(),
        task_settings(),
        64 * 1024 * 1024 * 1024,
        RetryPolicy::default(),
        false,
        orphan,
    )
    .unwrap()
}

fn write_older(path: &Path, bytes: &[u8], modified: SystemTime) {
    std::fs::write(path, bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn local_path(uri: &str) -> PathBuf {
    PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri))
}

/// Copies the fixture's manifest list to an old, unreferenced file in the
/// metadata directory, standing in for a proposal that never landed.
fn unresolved_manifest_list(run: &Run) -> PathBuf {
    let metadata = run.fixture.root().join("metadata");
    let source = std::fs::read_dir(&metadata)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("avro"))
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("snap-"))
        })
        .expect("the fixture writes a manifest list");
    let copy = metadata.join("unresolved-proposal.avro");
    std::fs::copy(source, &copy).unwrap();
    let two_days_ago = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
    std::fs::File::options()
        .write(true)
        .open(&copy)
        .unwrap()
        .set_modified(two_days_ago)
        .unwrap();
    copy
}

async fn insert_unresolved_intent(conn: &mut sqlx::PgConnection, job: JobId, manifest_list: &str) {
    sqlx::query("INSERT INTO publications (id, job_id) VALUES ($1, $2)")
        .bind(uuid::Uuid::now_v7())
        .bind(job.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO publication_intents
             (job_id, publication_id, content, request_body, request_hash)
         SELECT $1, id, $2, $3, $4 FROM publications WHERE job_id = $1",
    )
    .bind(job.as_uuid())
    .bind(serde_json::json!({ "manifest_list": manifest_list }))
    .bind(Vec::<u8>::new())
    .bind(digest('d'))
    .execute(&mut *conn)
    .await
    .unwrap();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_cleanup_plan_deletes_only_the_unreferenced_objects(pool: PgPool) {
    let mut run = setup(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let planned = planner(&run, enabled(50)).pass().await.unwrap();
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
    assert!(!candidates.iter().any(|path| path.contains("/bergsmith/")));

    let cleaned = publication(&run, enabled(50)).pass().await.unwrap();
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
    for output in &run.bergsmith_outputs {
        assert!(
            output.exists(),
            "Bergsmith's own tree is never removed: {}",
            output.display()
        );
    }
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
async fn a_replaced_object_is_skipped_instead_of_deleted(pool: PgPool) {
    let mut run = setup(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    planner(&run, enabled(50)).pass().await.unwrap();
    // The plan named an old object; replacing it makes the path young and a
    // different size, so deletion has to skip it.
    std::fs::write(&run.orphan_data, b"a much newer replacement object").unwrap();
    let cleaned = publication(&run, enabled(50)).pass().await.unwrap();
    assert_eq!(cleaned.cleaned, 1);
    assert!(
        run.orphan_data.exists(),
        "a replaced object is not the object the plan named"
    );
    assert!(!run.orphan_metadata.exists());
    let job = jobs::job(&mut conn, run.tenant.organization, run.job.id)
        .await
        .unwrap();
    assert_eq!(job.state, JobState::Succeeded);
    let receipts: Vec<(String, String)> =
        sqlx::query_as("SELECT path, disposition FROM orphan_deletions WHERE job_id = $1")
            .bind(job.id.as_uuid())
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    assert!(receipts.contains(&(
        file_uri(&run.orphan_data),
        Disposition::Skipped.as_str().to_owned()
    )));
    run.server.reset();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn partial_batches_resume_from_receipts_and_complete(pool: PgPool) {
    let mut run = setup(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    planner(&run, enabled(1)).pass().await.unwrap();
    let first = publication(&run, enabled(1)).pass().await.unwrap();
    assert_eq!(first.cleaned, 1);
    let job = jobs::job(&mut conn, run.tenant.organization, run.job.id)
        .await
        .unwrap();
    assert_eq!(job.state, JobState::Deleting, "a bounded batch leaves work");
    assert_eq!(
        orphans::receipt_totals(&mut conn, job.id).await.unwrap().0,
        1
    );
    assert!(!run.orphan_data.exists());
    assert!(run.orphan_metadata.exists(), "the second object waits");

    let second = publication(&run, enabled(1)).pass().await.unwrap();
    assert_eq!(second.cleaned, 1);
    let job = jobs::job(&mut conn, run.tenant.organization, run.job.id)
        .await
        .unwrap();
    assert_eq!(job.state, JobState::Succeeded);
    assert!(!run.orphan_metadata.exists());
    run.server.reset();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_cancellation_between_batches_stops_deletion(pool: PgPool) {
    let mut run = setup(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    planner(&run, enabled(1)).pass().await.unwrap();
    publication(&run, enabled(1)).pass().await.unwrap();
    let outcome = cancellation::request_cancellation(
        &mut conn,
        &CancelJob {
            job_id: run.job.id,
            requested_by: Some(run.tenant.user),
            reason: Some("test".to_owned()),
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.disposition, CancellationDisposition::Cancelling);
    assert!(run.orphan_metadata.exists());
    publication(&run, enabled(1)).pass().await.unwrap();
    assert!(
        run.orphan_metadata.exists(),
        "cancellation stops the remaining batches"
    );
    let job = jobs::job(&mut conn, run.tenant.organization, run.job.id)
        .await
        .unwrap();
    assert_eq!(job.state, JobState::Cancelling);
    run.server.reset();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_permanent_scan_failure_terminates_the_deleting_job(pool: PgPool) {
    let mut run = setup(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    planner(&run, enabled(50)).pass().await.unwrap();
    let too_small = OrphanConfig {
        max_scan_bytes: 1,
        ..enabled(50)
    };
    let failed = publication(&run, too_small).pass().await.unwrap();
    assert_eq!(failed.failed, 1);
    let job = jobs::job(&mut conn, run.tenant.organization, run.job.id)
        .await
        .unwrap();
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(job.state_reason.as_deref(), Some("OrphanCleanupInvalid"));
    assert!(run.orphan_data.exists(), "a failed pass deletes nothing");
    run.server.reset();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_object_removed_before_the_batch_is_recorded_absent(pool: PgPool) {
    let mut run = setup(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    planner(&run, enabled(50)).pass().await.unwrap();
    // Simulate a crash between the delete and its receipt: the object is gone
    // but the ledger never heard about it.
    std::fs::remove_file(&run.orphan_data).unwrap();
    publication(&run, enabled(50)).pass().await.unwrap();
    let job = jobs::job(&mut conn, run.tenant.organization, run.job.id)
        .await
        .unwrap();
    assert_eq!(job.state, JobState::Succeeded);
    let receipts: Vec<(String, String)> =
        sqlx::query_as("SELECT path, disposition FROM orphan_deletions WHERE job_id = $1")
            .bind(job.id.as_uuid())
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    assert!(receipts.contains(&(
        file_uri(&run.orphan_data),
        Disposition::Absent.as_str().to_owned()
    )));
    assert!(!run.orphan_metadata.exists());
    run.server.reset();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_former_leader_deletes_nothing(pool: PgPool) {
    let mut run = setup(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    planner(&run, enabled(50)).pass().await.unwrap();
    // Let the lease lapse and hand it to another coordinator.
    sqlx::query(
        "UPDATE coordinator_leadership
         SET acquired_at = now() - interval '2 hours',
             lease_expires_at = now() - interval '1 hour'",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    let successor = acquire_leadership(&mut conn, "successor", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("the lease is free");
    assert_ne!(successor.epoch, run.epoch);
    let deferred = publication(&run, enabled(50)).pass().await.unwrap();
    assert_eq!(deferred.deferred, 1);
    let job = jobs::job(&mut conn, run.tenant.organization, run.job.id)
        .await
        .unwrap();
    assert_eq!(job.state, JobState::ReadyToCommit);
    assert!(run.orphan_data.exists());
    assert!(run.orphan_metadata.exists());
    run.server.reset();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unresolved_publication_keeps_its_manifests(pool: PgPool) {
    let mut run = setup(&pool).await;
    let list = unresolved_manifest_list(&run);
    let mut conn = pool.acquire().await.unwrap();
    let pending = jobs::create_job(
        &mut conn,
        &NewJob {
            id: JobId::generate(),
            organization_id: run.tenant.organization,
            table_id: run.tenant.table,
            operation: JobOperation::Compact,
            retry_of: None,
            created_by: run.tenant.user,
        },
    )
    .await
    .unwrap();
    force_job_state(&mut conn, pending.id, JobState::Committing).await;
    insert_unresolved_intent(&mut conn, pending.id, &file_uri(&list)).await;

    planner(&run, enabled(50)).pass().await.unwrap();
    let (content, _hash) = orphans::orphan_plan(&mut conn, run.job.id)
        .await
        .unwrap()
        .unwrap();
    let document = OrphanPlanDocument::from_persisted(content).unwrap();
    assert!(
        document
            .candidates
            .iter()
            .all(|candidate| candidate.path != file_uri(&list)),
        "the unresolved proposal's manifest list is protected"
    );
    publication(&run, enabled(50)).pass().await.unwrap();
    assert!(
        list.exists(),
        "the unresolved proposal's manifest list survives cleanup"
    );
    run.server.reset();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_candidate_that_becomes_protected_is_skipped(pool: PgPool) {
    let mut run = setup(&pool).await;
    // A copy of the manifest list is an unreferenced, old metadata object, so
    // planning names it as a candidate.
    let candidate = unresolved_manifest_list(&run);
    let mut conn = pool.acquire().await.unwrap();
    planner(&run, enabled(50)).pass().await.unwrap();
    let (content, _hash) = orphans::orphan_plan(&mut conn, run.job.id)
        .await
        .unwrap()
        .unwrap();
    let document = OrphanPlanDocument::from_persisted(content).unwrap();
    assert!(
        document
            .candidates
            .iter()
            .any(|planned| planned.path == file_uri(&candidate)),
        "the unreferenced manifest list is planned for deletion"
    );

    // A proposal that appears after planning names that object, so the batch
    // must re-verify against the ledger and skip it.
    let pending = jobs::create_job(
        &mut conn,
        &NewJob {
            id: JobId::generate(),
            organization_id: run.tenant.organization,
            table_id: run.tenant.table,
            operation: JobOperation::Compact,
            retry_of: None,
            created_by: run.tenant.user,
        },
    )
    .await
    .unwrap();
    force_job_state(&mut conn, pending.id, JobState::Committing).await;
    insert_unresolved_intent(&mut conn, pending.id, &file_uri(&candidate)).await;

    publication(&run, enabled(50)).pass().await.unwrap();
    assert!(candidate.exists(), "the newly protected object survives");
    let receipts: Vec<(String, String)> =
        sqlx::query_as("SELECT path, disposition FROM orphan_deletions WHERE job_id = $1")
            .bind(run.job.id.as_uuid())
            .fetch_all(&mut *conn)
            .await
            .unwrap();
    assert!(receipts.contains(&(
        file_uri(&candidate),
        Disposition::Skipped.as_str().to_owned()
    )));
    run.server.reset();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_resumed_job_without_a_plan_fails(pool: PgPool) {
    let mut run = setup(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    force_job_state(&mut conn, run.job.id, JobState::Deleting).await;
    let summary = publication(&run, enabled(50)).pass().await.unwrap();
    assert_eq!(summary.failed, 1);
    let job = jobs::job(&mut conn, run.tenant.organization, run.job.id)
        .await
        .unwrap();
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(job.state_reason.as_deref(), Some("PlanMissing"));
    run.server.reset();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_undecodable_plan_fails_instead_of_looping(pool: PgPool) {
    let mut run = setup(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    force_job_state(&mut conn, run.job.id, JobState::ReadyToCommit).await;
    sqlx::query(
        "INSERT INTO orphan_plans (id, job_id, content, content_hash) VALUES ($1, $2, $3, $4)",
    )
    .bind(PlanId::generate().as_uuid())
    .bind(run.job.id.as_uuid())
    .bind(serde_json::json!({}))
    .bind(digest('e'))
    .execute(&mut *conn)
    .await
    .unwrap();
    let summary = publication(&run, enabled(50)).pass().await.unwrap();
    assert_eq!(summary.failed, 1);
    let job = jobs::job(&mut conn, run.tenant.organization, run.job.id)
        .await
        .unwrap();
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(
        job.state_reason.as_deref(),
        Some("PublicationPlanInvalid"),
        "a corrupt plan terminates the job instead of looping"
    );
    run.server.reset();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cleanup_is_refused_when_disabled(pool: PgPool) {
    let mut run = setup(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let summary = planner(&run, OrphanConfig::default()).pass().await.unwrap();
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
