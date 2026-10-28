//! BS-031.T3: manifest rewrite and dangling-delete removal, planned and
//! published by the serve runtime against native Iceberg fixtures.

// Fixtures fail the test immediately; helper functions are not `#[test]` items,
// so clippy's allow-*-in-tests settings do not cover them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use bergsmith_catalog::{CommitRequest, ObservedTable, TableName};
use bergsmith_common::ids::{
    CatalogConnectionId, JobId, OrganizationId, PrincipalId, RoleGrantId, TableId,
};
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_coordinator::durable_publication;
use bergsmith_coordinator::inventory::record_observation;
use bergsmith_coordinator::runtime::PublicationService;
use bergsmith_coordinator::runtime::catalogs::{RuntimeCatalogs, RuntimeWorkerAccess};
use bergsmith_coordinator::runtime::config::{
    DeleteSettings, ManifestSettings, MetadataRewriteConfig, OrphanConfig, RuntimeProfiles,
    TaskSettings,
};
use bergsmith_coordinator::runtime::planning::PlannerService;
use bergsmith_coordinator::scheduling::{Scheduler, SchedulerConfig};
use bergsmith_coordinator::workers::{WorkerConfig, WorkerService};
use bergsmith_fixtures::{Commit, FileSpec, Fixture, TableSpec, row};
use bergsmith_maintenance::PlannerConfig;
use bergsmith_maintenance::metadata_plan::{MetadataPlanDocument, MetadataWork};
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::jobs::{self, NewJob};
use bergsmith_persistence::scheduling::RetryPolicy;
use bergsmith_persistence::tenancy::{
    self, NewCatalogConnection, NewOrganization, NewPrincipal, PrincipalKind,
};
use bergsmith_persistence::{Database, DatabaseConfig, DatabaseUrl, publications};
use bergsmith_storage::{Location, StorageProfile, StorageProfiles};
use iceberg::spec::{
    DataContentType, FormatVersion, ManifestContentType, ManifestList, NestedField, PrimitiveType,
    Schema, TableMetadata, Type,
};
use mockito::{Matcher, Mock, Server, ServerGuard};
use serde_json::json;
use sqlx::{ConnectOptions, PgPool};

const TABLE_PATH: &str = "/v1/namespaces/db/tables/events";

/// An equality delete committed before any data (dangling), then two data
/// commits, the second with a position delete on the first's file (live).
/// The table has two small data manifests.
async fn fixture() -> Fixture {
    let schema = Schema::builder()
        .with_fields([Arc::new(NestedField::required(
            1,
            "id",
            Type::Primitive(PrimitiveType::Long),
        ))])
        .build()
        .unwrap();
    TableSpec::new(schema)
        .commit(Commit::new().file(FileSpec::equality_deletes(
            "early",
            [1],
            [row![1 => 99_i64]],
        )))
        .commit(Commit::new().file(FileSpec::data("a", (0..3_i64).map(|id| row![1 => id]))))
        .commit(
            Commit::new()
                .file(FileSpec::data("b", [row![1 => 7_i64]]))
                .file(FileSpec::position_deletes("a-deletes", [("a", 0)])),
        )
        .generate()
        .await
        .unwrap()
}

/// A table with one data commit: nothing dangles and nothing merges.
async fn clean_fixture() -> Fixture {
    let schema = Schema::builder()
        .with_fields([Arc::new(NestedField::required(
            1,
            "id",
            Type::Primitive(PrimitiveType::Long),
        ))])
        .build()
        .unwrap();
    TableSpec::new(schema)
        .commit(Commit::new().file(FileSpec::data("only", [row![1 => 1_i64]])))
        .generate()
        .await
        .unwrap()
}

fn enabled() -> MetadataRewriteConfig {
    MetadataRewriteConfig::new(
        &ManifestSettings {
            rewrite_enabled: true,
            ..ManifestSettings::default()
        },
        &DeleteSettings {
            dangling_removal_enabled: true,
            ..DeleteSettings::default()
        },
    )
    .unwrap()
}

struct Env {
    pool: PgPool,
    server: ServerGuard,
    fixture: Fixture,
    load: Mock,
    organization: OrganizationId,
    table: TableId,
    created_by: PrincipalId,
    epoch: i64,
    catalogs: Arc<RuntimeCatalogs>,
}

async fn load_mock(server: &mut Server, fixture: &Fixture, metadata: &TableMetadata) -> Mock {
    load_mock_at(server, fixture.metadata_location(), metadata).await
}

async fn load_mock_at(server: &mut Server, location: &str, metadata: &TableMetadata) -> Mock {
    server
        .mock("GET", TABLE_PATH)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            json!({
                "metadata-location": location,
                "metadata": metadata,
                "config": {}
            })
            .to_string(),
        )
        .create_async()
        .await
}

/// Registers an organization, a catalog connection at `url`, the fixture's
/// table, and an administrator.
async fn register(
    conn: &mut sqlx::PgConnection,
    url: &str,
    fixture: &Fixture,
) -> (OrganizationId, TableId, PrincipalId) {
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
            uri: url.to_owned(),
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
    let table = record_observation(conn, organization, connection, &observed)
        .await
        .unwrap();
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
    (organization, table, created_by)
}

impl Env {
    async fn new(pool: &PgPool, fixture: Fixture) -> Self {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/v1/config")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"defaults":{},"overrides":{}}"#)
            .create_async()
            .await;
        let load = load_mock(&mut server, &fixture, fixture.metadata()).await;
        let mut conn = pool.acquire().await.unwrap();
        let (organization, table, created_by) = register(&mut conn, &server.url(), &fixture).await;
        let epoch = bergsmith_persistence::leadership::acquire_leadership(
            &mut conn,
            "runtime-test",
            std::time::Duration::from_secs(300),
        )
        .await
        .unwrap()
        .expect("leadership is acquired")
        .epoch;
        drop(conn);
        let mut settings = DatabaseConfig::new(DatabaseUrl::new(
            pool.connect_options().to_url_lossy().to_string(),
        ));
        settings.max_connections = 1;
        let database = Database::connect(&settings).await.unwrap();
        let catalogs = Arc::new(RuntimeCatalogs::new(
            database,
            RuntimeProfiles {
                default: StorageProfiles::new([StorageProfile::LocalFs]),
                connections: BTreeMap::new(),
            },
        ));
        Self {
            pool: pool.clone(),
            server,
            fixture,
            load,
            organization,
            table,
            created_by,
            epoch,
            catalogs,
        }
    }

    async fn job(&self, operation: JobOperation) -> JobId {
        let mut conn = self.pool.acquire().await.unwrap();
        jobs::create_job(
            &mut conn,
            &NewJob {
                id: JobId::generate(),
                organization_id: self.organization,
                table_id: self.table,
                operation,
                retry_of: None,
                created_by: self.created_by,
            },
        )
        .await
        .unwrap()
        .id
    }

    async fn current(&self, job: JobId) -> jobs::Job {
        jobs::job(
            &mut self.pool.acquire().await.unwrap(),
            self.organization,
            job,
        )
        .await
        .unwrap()
    }

    fn planner(&self, metadata: MetadataRewriteConfig) -> PlannerService {
        let access = Arc::new(RuntimeWorkerAccess::new(self.catalogs.clone()));
        let worker = WorkerService::new(
            self.pool.clone(),
            self.epoch,
            WorkerConfig::default(),
            access.clone(),
        )
        .unwrap();
        let scheduler =
            Arc::new(Scheduler::new(worker, access, SchedulerConfig::default()).unwrap());
        PlannerService::new(
            self.pool.clone(),
            self.catalogs.clone(),
            scheduler,
            PlannerConfig::default(),
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
            true,
            OrphanConfig::default(),
        )
        .unwrap()
        .with_metadata_rewrites(metadata)
    }

    fn publisher(&self, metadata: MetadataRewriteConfig) -> PublicationService {
        PublicationService::new(
            self.pool.clone(),
            self.catalogs.clone(),
            self.epoch,
            durable_publication::PublicationConfig::default(),
            RetryPolicy::default(),
            true,
            OrphanConfig::default(),
        )
        .with_metadata_rewrites(metadata)
    }

    async fn plan(&self, job: JobId) -> MetadataPlanDocument {
        durable_publication::load_metadata_plan(&mut self.pool.acquire().await.unwrap(), job)
            .await
            .unwrap()
    }

    /// The table after applying the job's persisted request, as a catalog
    /// would.
    async fn published(&self, job: JobId) -> TableMetadata {
        let intent = publications::intent(&mut self.pool.acquire().await.unwrap(), job)
            .await
            .unwrap();
        let request = CommitRequest::from_persisted(
            TableName {
                namespace: vec!["db".into()],
                name: "events".into(),
            },
            intent.request_body,
        );
        let decoded = request.decode().unwrap();
        for requirement in &decoded.requirements {
            requirement.check(Some(self.fixture.metadata())).unwrap();
        }
        let mut builder = self.fixture.metadata().clone().into_builder(None);
        for update in decoded.updates {
            builder = update.apply(builder).unwrap();
        }
        builder.build().unwrap().metadata
    }

    /// The live `(content, path)` entries and the data manifest count of the
    /// current snapshot of `metadata`.
    async fn live(&self, metadata: &TableMetadata) -> (Vec<(DataContentType, String)>, usize) {
        let io = self.fixture.file_io();
        let snapshot = metadata.current_snapshot().unwrap();
        let bytes = io
            .new_input(snapshot.manifest_list())
            .unwrap()
            .read()
            .await
            .unwrap();
        let list = ManifestList::parse_with_version(&bytes, FormatVersion::V2).unwrap();
        let mut live = Vec::new();
        let mut data_manifests = 0;
        for manifest in list.entries() {
            if manifest.content == ManifestContentType::Data {
                data_manifests += 1;
            }
            for entry in manifest.load_manifest(io).await.unwrap().entries() {
                if entry.is_alive() {
                    live.push((
                        entry.data_file().content_type(),
                        entry.data_file().file_path().to_owned(),
                    ));
                }
            }
        }
        live.sort_by(|a, b| a.1.cmp(&b.1));
        (live, data_manifests)
    }

    async fn commit_mock(&mut self, status: usize) -> Mock {
        self.server
            .mock("POST", TABLE_PATH)
            .match_header("idempotency-key", Matcher::Missing)
            .with_status(status)
            .with_header("content-type", "application/json")
            .with_body("{}")
            .expect(1)
            .create_async()
            .await
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn dangling_deletes_are_planned_and_removed_in_one_replace_snapshot(pool: PgPool) {
    let mut env = Env::new(&pool, fixture().await).await;
    let (before, _) = env.live(env.fixture.metadata()).await;
    let job = env.job(JobOperation::RemoveDanglingDeletes).await;

    let summary = env.planner(enabled()).pass().await.unwrap();
    assert_eq!(summary.planned, 1, "{summary:?}");
    assert_eq!(summary.registered, 0);
    assert_eq!(env.current(job).await.state, JobState::ReadyToCommit);
    let plan = env.plan(job).await;
    let MetadataWork::RemoveDanglingDeletes {
        deletes,
        examined,
        deferred,
    } = &plan.work
    else {
        panic!("unexpected work {:?}", plan.work);
    };
    assert_eq!(*examined, 2);
    assert_eq!(*deferred, 0);
    let early = env.fixture.path_of("early").unwrap().to_owned();
    assert_eq!(
        deletes.iter().map(|d| d.path.as_str()).collect::<Vec<_>>(),
        [early.as_str()]
    );

    let commit = env.commit_mock(200).await;
    let summary = env.publisher(enabled()).pass().await.unwrap();
    assert_eq!(summary.published, 1, "{summary:?}");
    commit.assert_async().await;
    let done = env.current(job).await;
    assert_eq!(done.state, JobState::Succeeded);

    let intent = publications::intent(&mut pool.acquire().await.unwrap(), job)
        .await
        .unwrap();
    assert_eq!(intent.content["receipts"], json!([]));
    assert_eq!(
        intent.content["plan"]["baseline"]["snapshot_id"],
        json!(plan.baseline.snapshot_id())
    );
    let published = env.published(job).await;
    let snapshot = published.current_snapshot().unwrap();
    assert_eq!(
        snapshot.summary().operation,
        iceberg::spec::Operation::Replace
    );
    assert_eq!(
        snapshot.summary().additional_properties["bergsmith.job-id"],
        job.to_string()
    );
    let (after, _) = env.live(&published).await;
    let expected: Vec<_> = before
        .into_iter()
        .filter(|(_, path)| *path != early)
        .collect();
    assert_eq!(after, expected);
    env.load.remove_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn small_manifests_are_planned_and_merged_without_changing_files(pool: PgPool) {
    let mut env = Env::new(&pool, fixture().await).await;
    let (before, manifests) = env.live(env.fixture.metadata()).await;
    assert_eq!(manifests, 2);
    let job = env.job(JobOperation::RewriteManifests).await;

    let summary = env.planner(enabled()).pass().await.unwrap();
    assert_eq!(summary.planned, 1, "{summary:?}");
    let plan = env.plan(job).await;
    let MetadataWork::RewriteManifests {
        manifests,
        target_size_bytes,
        ..
    } = &plan.work
    else {
        panic!("unexpected work {:?}", plan.work);
    };
    assert_eq!(manifests.len(), 2);
    assert_eq!(*target_size_bytes, 8 * 1024 * 1024);

    let commit = env.commit_mock(200).await;
    let summary = env.publisher(enabled()).pass().await.unwrap();
    assert_eq!(summary.published, 1, "{summary:?}");
    commit.assert_async().await;
    assert_eq!(env.current(job).await.state, JobState::Succeeded);
    let published = env.published(job).await;
    let (after, data_manifests) = env.live(&published).await;
    assert_eq!(data_manifests, 1);
    assert_eq!(after, before);
    env.load.remove_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_table_with_nothing_to_rewrite_ends_no_op(pool: PgPool) {
    let env = Env::new(&pool, clean_fixture().await).await;
    let manifests = env.job(JobOperation::RewriteManifests).await;
    let deletes = env.job(JobOperation::RemoveDanglingDeletes).await;

    let summary = env.planner(enabled()).pass().await.unwrap();
    assert_eq!(summary.no_op, 2, "{summary:?}");
    let manifests = env.current(manifests).await;
    assert_eq!(manifests.state, JobState::NoOp);
    assert_eq!(
        manifests.state_reason.as_deref(),
        Some("no_manifests_to_rewrite")
    );
    let deletes = env.current(deletes).await;
    assert_eq!(deletes.state, JobState::NoOp);
    assert_eq!(deletes.state_reason.as_deref(), Some("no_dangling_deletes"));
    env.load.remove_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn disabled_operations_fail_closed_at_planning_and_publication(pool: PgPool) {
    let env = Env::new(&pool, fixture().await).await;
    let manifests = env.job(JobOperation::RewriteManifests).await;
    let deletes = env.job(JobOperation::RemoveDanglingDeletes).await;

    let summary = env
        .planner(MetadataRewriteConfig::default())
        .pass()
        .await
        .unwrap();
    assert_eq!(summary.failed, 2, "{summary:?}");
    let manifests = env.current(manifests).await;
    assert_eq!(manifests.state, JobState::Failed);
    assert_eq!(
        manifests.state_reason.as_deref(),
        Some("ManifestRewriteUnavailable")
    );
    let deletes = env.current(deletes).await;
    assert_eq!(
        deletes.state_reason.as_deref(),
        Some("DanglingDeleteRemovalUnavailable")
    );

    // A plan made while enabled does not publish once the switch is off.
    let planned = env.job(JobOperation::RemoveDanglingDeletes).await;
    env.planner(enabled()).pass().await.unwrap();
    assert_eq!(env.current(planned).await.state, JobState::ReadyToCommit);
    let summary = env
        .publisher(MetadataRewriteConfig::default())
        .pass()
        .await
        .unwrap();
    assert_eq!(summary.failed, 1, "{summary:?}");
    let stopped = env.current(planned).await;
    assert_eq!(stopped.state, JobState::Failed);
    assert_eq!(
        stopped.state_reason.as_deref(),
        Some("DanglingDeleteRemovalUnavailable")
    );
    env.load.remove_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_changed_baseline_replans_instead_of_publishing(pool: PgPool) {
    let mut env = Env::new(&pool, fixture().await).await;
    let job = env.job(JobOperation::RewriteManifests).await;
    env.planner(enabled()).pass().await.unwrap();
    assert_eq!(env.current(job).await.state, JobState::ReadyToCommit);

    // Another writer moved the table to a new metadata file.
    env.load.remove_async().await;
    let moved = format!("{}.moved", env.fixture.metadata_location());
    env.load = load_mock_at(&mut env.server, &moved, env.fixture.metadata()).await;

    let summary = env.publisher(enabled()).pass().await.unwrap();
    assert_eq!(summary.replanned, 1, "{summary:?}");
    let current = env.current(job).await;
    assert_eq!(current.state, JobState::ReplanRequired);
    assert_eq!(
        current.state_reason.as_deref(),
        Some("PublicationBaselineChanged")
    );
    env.load.remove_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_ambiguous_rewrite_resolves_only_by_snapshot_evidence(pool: PgPool) {
    let mut env = Env::new(&pool, fixture().await).await;
    let job = env.job(JobOperation::RemoveDanglingDeletes).await;
    env.planner(enabled()).pass().await.unwrap();
    let commit = env.commit_mock(503).await;
    let summary = env.publisher(enabled()).pass().await.unwrap();
    assert_eq!(summary.unresolved, 1, "{summary:?}");
    commit.assert_async().await;
    let unknown = env.current(job).await;
    assert_eq!(unknown.state, JobState::CommitOutcomeUnknown);

    let catalog = bergsmith_catalog::CatalogConnection::connect(
        &bergsmith_catalog::ConnectionConfig::new(url::Url::parse(&env.server.url()).unwrap()),
    )
    .await
    .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let still = durable_publication::reconcile(&mut conn, &unknown, env.epoch, &catalog)
        .await
        .unwrap();
    assert_eq!(still.state, JobState::CommitOutcomeUnknown);

    let published = env.published(job).await;
    env.load.remove_async().await;
    env.load = load_mock(&mut env.server, &env.fixture, &published).await;
    let resolved = durable_publication::reconcile(&mut conn, &still, env.epoch, &catalog)
        .await
        .unwrap();
    assert_eq!(resolved.state, JobState::Succeeded);
    assert_eq!(
        resolved.state_reason.as_deref(),
        Some("SnapshotEvidenceMatched")
    );
    env.load.remove_async().await;
}
