//! Process acceptance support. Faults happen after the real durable RPC decision.

mod checkpoints;
mod native_gate;
mod peer;
mod publication;

/// The execution budget the tests plan with.
const EXECUTION_BUDGET: Duration = Duration::from_secs(180);

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use bergsmith_common::ids::{JobId, PlanId, PrincipalId};
use bergsmith_common::job::JobState;
use bergsmith_coordinator::{planning, scheduling::Scheduler, workers::TaskOffer};
use bergsmith_fixtures::{Commit, FileSpec, Fixture, TableSpec, Value, row};
use bergsmith_maintenance::{
    PlanDocument, PlanIds, PlanInput, PlanOutcome, Planner, PlannerConfig,
};
use bergsmith_persistence::{jobs, tables, tenancy};
use bergsmith_protocol::{cluster as wire, hashing::artifact_digest, identity::ServiceToken};
use bergsmith_storage::{Location, ObjectStore, StorageProfile};
use iceberg::spec::{NestedField, PrimitiveType, Schema, Transform, Type, UnboundPartitionSpec};
use prost::Message;
use serde::{Deserialize, Serialize};
use sqlx::{ConnectOptions, PgPool};
use tempfile::TempDir;
use tokio::net::UnixDatagram;
use tokio::process::{Child, Command};
use uuid::Uuid;

use crate::support;
pub(crate) use peer::coordinator;

const TOKEN: &str = "bsw1_native_abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMN-0";
const DEADLINE: Duration = Duration::from_secs(180);

#[derive(Serialize, Deserialize)]
struct Config {
    database: String,
    directory: PathBuf,
    metadata: iceberg::spec::TableMetadata,
    metadata_location: String,
    organization: bergsmith_common::ids::OrganizationId,
    principal: PrincipalId,
    catalog: String,
    job: JobId,
}

pub(crate) struct Harness {
    pool: PgPool,
    directory: TempDir,
    fixture: Fixture,
    tenant: support::Tenant,
    job: JobId,
    plan: PlanDocument,
    store: ObjectStore,
    coordinator: Option<Child>,
    workers: Vec<Child>,
    catalog: publication::Catalog,
    events: UnixDatagram,
    native_gate: Option<native_gate::NativeGate>,
    abandoned_output: Option<Location>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.directory.disable_cleanup(true);
        }
        // kill_on_drop is also armed, including while an assertion unwinds.
        for worker in &mut self.workers {
            let _ = worker.start_kill();
        }
        if let Some(child) = &mut self.coordinator {
            let _ = child.start_kill();
        }
    }
}

async fn fixture(groups: i32) -> Fixture {
    let schema = Schema::builder()
        .with_fields([
            Arc::new(NestedField::required(
                1,
                "group",
                Type::Primitive(PrimitiveType::Int),
            )),
            Arc::new(NestedField::optional(
                2,
                "value",
                Type::Primitive(PrimitiveType::String),
            )),
        ])
        .build()
        .unwrap();
    let spec = UnboundPartitionSpec::builder()
        .add_partition_field(1, "group", Transform::Identity)
        .unwrap()
        .build();
    let mut commit = Commit::new();
    for group in 0..groups {
        // Distinct partitions force exactly one group per partition. Duplicate
        // rows and nulls make a count-only or set-only oracle insufficient.
        let rows = [
            row! {1 => group, 2 => "duplicate"},
            row! {1 => group, 2 => "duplicate"},
            row! {1 => group, 2 => Value::Null},
        ];
        for file in 0..2 {
            commit = commit.file(
                FileSpec::data(format!("g{group}-{file}"), rows.clone())
                    .partition([Value::Int(group)]),
            );
        }
    }
    TableSpec::new(schema)
        .partitioned_by(spec)
        .commit(commit)
        .generate()
        .await
        .unwrap()
}

async fn plan(fixture: &Fixture, job: JobId) -> PlanOutcome {
    Planner::new(PlannerConfig {
        min_input_files: 2,
        min_files_removed: 1,
        task_overhead_millis: 1,
        ..PlannerConfig::default()
    })
    .unwrap()
    .plan(
        PlanInput {
            metadata: fixture.metadata(),
            metadata_location: Some(fixture.metadata_location()),
            file_io: fixture.file_io(),
            selection: &[],
        },
        PlanIds {
            job_id: job,
            plan_id: PlanId::generate(),
        },
    )
    .await
    .unwrap()
}

impl Harness {
    pub(crate) async fn new(pool: PgPool, groups: i32) -> Self {
        let fixture = fixture(groups).await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let events = UnixDatagram::bind(directory.path().join("events.sock")).unwrap();
        // This private, read-only token is created exactly once. Worker starts
        // must never truncate a credential another process is about to read.
        let mut token = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o400)
            .open(directory.path().join("token"))
            .unwrap();
        token.write_all(TOKEN.as_bytes()).unwrap();
        drop(token);
        peer::certificate(directory.path()).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut tenant = support::tenant(&mut conn, "distributed").await;
        tenant.table = tables::resolve_table(
            &mut conn,
            tenant.organization,
            &support::alias(tenant.connection, "native"),
            fixture.metadata().uuid(),
        )
        .await
        .unwrap()
        .id;
        let principal = PrincipalId::generate();
        tenancy::create_principal(
            &mut conn,
            &tenancy::NewPrincipal {
                id: principal,
                organization_id: tenant.organization,
                kind: tenancy::PrincipalKind::Service,
                issuer: "bergsmith:test".into(),
                subject: principal.to_string(),
                display_name: "process workers".into(),
            },
        )
        .await
        .unwrap();
        let job = support::job(&mut conn, &tenant).await;
        let outcome = plan(&fixture, job).await;
        let PlanOutcome::Planned(plan) = &outcome else {
            panic!("missing native plan: {outcome:?}")
        };
        assert_eq!(plan.tasks().len(), usize::try_from(groups).unwrap());
        let plan = plan.as_ref().clone();
        let saved = jobs::job(&mut conn, tenant.organization, job)
            .await
            .unwrap();
        planning::record_outcome(&mut conn, &saved, &outcome, EXECUTION_BUDGET)
            .await
            .unwrap();
        drop(conn);
        let store = ObjectStore::open(&StorageProfile::LocalFs, None).unwrap();
        let catalog = publication::Catalog::new(&fixture, directory.path()).await;
        let config = Config {
            database: pool.connect_options().to_url_lossy().to_string(),
            directory: directory.path().into(),
            metadata: fixture.metadata().clone(),
            metadata_location: fixture.metadata_location().into(),
            organization: tenant.organization,
            principal,
            catalog: catalog.url(),
            job,
        };
        std::fs::write(
            directory.path().join("coordinator.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let mut run = Self {
            pool,
            directory,
            fixture,
            tenant,
            job,
            plan,
            store,
            coordinator: None,
            workers: vec![],
            catalog,
            events,
            native_gate: None,
            abandoned_output: None,
        };
        run.register_dispatch(&config).await;
        run.start_coordinator().await;
        run.native_gate = Some(native_gate::NativeGate::new(
            run.fixture.metadata_location(),
        ));
        run.start_worker(0);
        run
    }

    async fn register_dispatch(&self, config: &Config) {
        let access = Arc::new(peer::Access::new(config).await);
        // Persist dispatch with the same public scheduler API used at startup.
        // No offer queue is copied into the child or across restarts.
        let epoch = support::leader(&mut self.pool.acquire().await.unwrap()).await;
        let service = peer::service(&self.pool, epoch, config, access.clone());
        let scheduler = Scheduler::new(service, access, peer::scheduler_config()).unwrap();
        let mut offers = Vec::new();
        for task in self.plan.tasks() {
            let artifact = self.plan.artifact_of(task).unwrap();
            let location = Location::parse(self.fixture.metadata().location())
                .unwrap()
                .child(&format!("{}.pb", task.task_id()))
                .unwrap();
            self.store
                .write(&location, artifact.encode_to_vec().into())
                .await
                .unwrap();
            offers.push(TaskOffer {
                organization: self.tenant.organization,
                task: task.task_id(),
                pool: "native".into(),
                capability: "native-v1".into(),
                artifact: wire::ArtifactRef {
                    uri: location.to_string(),
                    sha256: artifact_digest(&artifact).as_bytes().to_vec(),
                },
                limits: wire::ResourceLimits {
                    memory_bytes: 1024 * 1024 * 1024,
                    cpu_millis: 1000,
                    max_duration: Some(prost_types::Duration {
                        seconds: 60,
                        nanos: 0,
                    }),
                    max_output_bytes: 32 * 1024 * 1024,
                },
            });
        }
        scheduler.register(self.job, &offers).await.unwrap();
        drop(scheduler);
    }

    pub(crate) async fn assert_fresh_retry_plan(&self) {
        let mut conn = self.pool.acquire().await.unwrap();
        let terminal = jobs::job(&mut conn, self.tenant.organization, self.job)
            .await
            .unwrap();
        assert_eq!(terminal.state, JobState::Cancelled);
        let retry = jobs::create_job(
            &mut conn,
            &jobs::NewJob {
                id: JobId::generate(),
                organization_id: self.tenant.organization,
                table_id: self.tenant.table,
                retry_of: Some(self.job),
                created_by: self.tenant.user,
            },
        )
        .await
        .unwrap();
        let outcome = plan(&self.fixture, retry.id).await;
        let PlanOutcome::Planned(fresh) = &outcome else {
            panic!("retry must plan fresh execution")
        };
        assert_ne!(fresh.plan_id(), self.plan.plan_id());
        assert_eq!(fresh.tasks().len(), self.plan.tasks().len());
        for task in fresh.tasks() {
            assert!(
                self.plan
                    .tasks()
                    .iter()
                    .all(|old| old.task_id() != task.task_id())
            );
            let artifact = fresh.artifact_of(task).unwrap();
            assert_eq!(artifact.job_id, retry.id.to_string());
            assert_eq!(artifact.plan_id, fresh.plan_id().to_string());
        }
        planning::record_outcome(&mut conn, &retry, &outcome, EXECUTION_BUDGET)
            .await
            .unwrap();
        let counts: (i64, i64, i64) = sqlx::query_as("SELECT count(*),count(*) FILTER (WHERE state='Pending'),(SELECT count(*) FROM receipts r JOIN tasks t ON t.id=r.task_id WHERE t.job_id=$1) FROM tasks WHERE job_id=$1")
            .bind(retry.id.as_uuid()).fetch_one(&mut *conn).await.unwrap();
        let tasks = i64::try_from(fresh.tasks().len()).unwrap();
        assert_eq!(
            counts,
            (tasks, tasks, 0),
            "operator retry cannot reuse accepted results"
        );
        assert_eq!(retry.retry_of, Some(self.job));
        assert_eq!(
            jobs::job(&mut conn, self.tenant.organization, self.job)
                .await
                .unwrap(),
            terminal
        );
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    async fn start_coordinator(&mut self) {
        // Test-only clock advancement follows a confirmed process exit. A live
        // coordinator's lease is never stolen to induce the restart.
        sqlx::query("UPDATE coordinator_leadership SET lease_expires_at = greatest(acquired_at, clock_timestamp())").execute(&self.pool).await.unwrap();
        let _ = std::fs::remove_file(self.path("ready.json"));
        let log = std::fs::File::create(self.path(&format!("coordinator-{}.log", Uuid::now_v7())))
            .unwrap();
        self.coordinator = Some(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "coordinator_process", "--nocapture"])
                .env("BERGSMITH_RECOVERY_CHILD", self.path("coordinator.json"))
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .kill_on_drop(true)
                .spawn()
                .unwrap(),
        );
        self.wait_file("ready.json").await;
    }

    async fn restart_coordinator(&mut self) {
        let mut child = self.coordinator.take().unwrap();
        child.kill().await.unwrap();
        assert!(!child.wait().await.unwrap().success());
        self.start_coordinator().await;
    }

    fn start_worker(&mut self, index: usize) {
        let endpoint: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.path("ready.json")).unwrap()).unwrap();
        let token_path = self.path("token");
        let cgroup = std::env::var_os("BERGSMITH_TEST_CGROUP_ROOT").map(PathBuf::from);
        assert!(
            !cfg!(target_os = "linux") || cgroup.is_some(),
            "linux process acceptance requires BERGSMITH_TEST_CGROUP_ROOT (delegated memory/cpu/pids)"
        );
        let config = worker_config(
            self.directory.path(),
            &endpoint["endpoint"],
            index,
            cgroup.as_deref(),
        );
        let config_path = self.path(&format!("worker-{index}.json"));
        std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let log =
            std::fs::File::create(self.path(&format!("worker-{index}-{}.log", Uuid::now_v7())))
                .unwrap();
        let binary = std::env::var_os("BERGSMITH_TEST_WORKER_BIN")
            .expect("run make test-distributed to build the same-source worker");
        self.workers.push(
            Command::new(binary)
                .arg("run")
                .arg("--config")
                .arg(config_path)
                .arg("--token-file")
                .arg(token_path)
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .kill_on_drop(true)
                .spawn()
                .unwrap(),
        );
    }

    async fn command(&self, command: &str) {
        self.events
            .send_to(command.as_bytes(), self.path("commands.sock"))
            .await
            .unwrap();
    }

    async fn wait_file(&mut self, name: &str) {
        let path = self.path(name);
        let result = tokio::time::timeout(DEADLINE, async {
            let mut events = [0; 128];
            let mut health = tokio::time::interval(Duration::from_millis(100));
            while !path.exists() {
                self.assert_alive();
                tokio::select! {
                    result = self.events.recv(&mut events) => { result.unwrap(); },
                    _ = health.tick() => {},
                }
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "missing process checkpoint {name}; logs: {}",
            self.directory.path().display()
        );
    }

    fn assert_alive(&mut self) {
        if let Some(child) = &mut self.coordinator {
            assert!(
                child.try_wait().unwrap().is_none(),
                "coordinator exited; logs: {}",
                self.directory.path().display()
            );
        }
        for child in &mut self.workers {
            assert!(
                child.try_wait().unwrap().is_none(),
                "worker exited; logs: {}",
                self.directory.path().display()
            );
        }
    }

    pub(crate) async fn exercise_failures(&mut self) {
        self.wait_file("grant-replayed.pb").await;
        let original = wire::RequestTaskResponse::decode(
            std::fs::read(self.path("grant-lost.pb"))
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        let replay = wire::RequestTaskResponse::decode(
            std::fs::read(self.path("grant-replayed.pb"))
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        assert_eq!(
            original, replay,
            "lost poll response must replay the original grant"
        );
        let Some(wire::request_task_response::Result::Grant(grant)) = original.result else {
            panic!("missing persisted grant")
        };
        let gate = self.native_gate.take().unwrap();
        let metadata_writer = gate.reader(self).await;
        let task_pid = native_gate::child(self.workers[0].id().unwrap()).await;
        let abandoned = Location::parse(&grant.output_prefix)
            .unwrap()
            .child("interrupted.parquet")
            .unwrap();
        self.store
            .write(
                &abandoned,
                b"partial output from interrupted attempt".as_slice().into(),
            )
            .await
            .unwrap();
        self.abandoned_output = Some(abandoned);
        let mut dead = self.workers.remove(0);
        dead.kill().await.unwrap();
        assert!(!dead.wait().await.unwrap().success());
        #[cfg(not(target_os = "linux"))]
        gate.release_reader(metadata_writer).await;
        native_gate::exited(task_pid, self.directory.path()).await;
        #[cfg(target_os = "linux")]
        drop(metadata_writer);
        gate.restore();
        // Advance only the dead worker's attempt lease, rather than sleeping
        // for wall-clock expiry or altering the result of a live operation.
        support::expire_lease(
            &mut self.pool.acquire().await.unwrap(),
            bergsmith_common::ids::AttemptId::from_uuid(
                Uuid::parse_str(&grant.attempt_id).unwrap(),
            ),
        )
        .await;
        std::fs::write(self.path("grant-released"), []).unwrap();
        self.restart_coordinator().await;
        for worker in 0..3 {
            self.start_worker(worker);
        }
        // Every worker receives a real durable grant before execution proceeds.
        for worker in 0..3 {
            self.wait_file(&format!("started-worker-{worker}")).await;
        }
        std::fs::write(self.path("execution-released"), []).unwrap();
        self.command("execute").await;
        self.wait_file("result-lost.pb").await;
        self.restart_coordinator().await;
        self.wait_file("result-replayed.pb").await;
        assert_eq!(
            std::fs::read(self.path("result-lost.pb")).unwrap(),
            std::fs::read(self.path("result-replayed.pb")).unwrap(),
            "report retry must use the immutable receipt"
        );
    }

    pub(crate) async fn assert_execution(&mut self) {
        self.wait_file("execution-complete").await;
        let states: (i64, i64) = sqlx::query_as("SELECT count(*) FILTER (WHERE state='Accepted'),count(*) FILTER (WHERE state='Failed') FROM tasks WHERE job_id=$1")
            .bind(self.job.as_uuid()).fetch_one(&self.pool).await.unwrap();
        assert_eq!(states, (i64::try_from(self.plan.tasks().len()).unwrap(), 0));
        assert!(
            self.store
                .exists(self.abandoned_output.as_ref().unwrap())
                .await
                .unwrap(),
            "abandoned output is retained but never accepted"
        );
        let counts: (i64, i64, i64) = sqlx::query_as("SELECT count(*),count(DISTINCT r.task_id),count(DISTINCT w.worker_name) FROM receipts r JOIN tasks t ON t.id=r.task_id JOIN attempts a ON a.id=r.attempt_id JOIN worker_sessions w ON w.id=a.worker_session_id WHERE t.job_id=$1")
            .bind(self.job.as_uuid()).fetch_one(&self.pool).await.unwrap();
        let tasks = i64::try_from(self.plan.tasks().len()).unwrap();
        assert_eq!(counts, (tasks, tasks, 3));
        let attempts: (i64,i32) = sqlx::query_as("SELECT count(*),max(generation) FROM attempts a JOIN tasks t ON a.task_id=t.id WHERE t.job_id=$1").bind(self.job.as_uuid()).fetch_one(&self.pool).await.unwrap();
        assert_eq!(
            attempts,
            (tasks + 1, 2),
            "only the dead grant gets replacement execution"
        );
        for worker in &mut self.workers {
            worker.kill().await.unwrap();
            worker.wait().await.unwrap();
        }
        self.workers.clear();
    }
}

fn worker_config(
    directory: &Path,
    endpoint: &serde_json::Value,
    index: usize,
    cgroup: Option<&Path>,
) -> serde_json::Value {
    serde_json::json!({
        "endpoint": endpoint, "ca_certificate": directory.join("cert.pem"),
        "worker_name": format!("worker-{index}"), "pool": "native", "capabilities": ["native-v1"],
        "memory_bytes": 1024_u64 * 1024 * 1024, "cpu_millis": 1000,
        "state_directory": directory.join(format!("worker-{index}")), "cgroup_root": cgroup,
    })
}

#[test]
fn configured_cgroup_paths_round_trip_as_worker_paths() {
    #[derive(Deserialize)]
    struct Paths {
        cgroup_root: Option<PathBuf>,
    }
    for expected in [None, Some(PathBuf::from("/sys/fs/cgroup/worker tests"))] {
        let config = worker_config(
            Path::new("/tmp/worker"),
            &serde_json::json!("https://localhost:7443"),
            0,
            expected.as_deref(),
        );
        let decoded: Paths = serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
        assert_eq!(decoded.cgroup_root, expected);
    }
}
