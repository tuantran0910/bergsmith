use super::checkpoints::checkpoint;
use super::*;
use bergsmith_catalog::{CatalogConnection, ConnectionConfig};
use bergsmith_coordinator::{
    durable_publication,
    scheduling::{Eligibility, RetryPolicy, SchedulerConfig, SchedulingAccess},
    workers::{TaskAccess, WorkerAccess, WorkerConfig, WorkerService, worker_tls_config},
};
use bergsmith_persistence::leadership;
use bergsmith_protocol::cluster::cluster_service_server::{ClusterService, ClusterServiceServer};
use std::sync::atomic::{AtomicBool, Ordering};
use tonic::transport::{Identity, Server};
use tonic::{Request, Response, Status};

pub(super) struct Access {
    metadata: iceberg::spec::TableMetadata,
    metadata_location: String,
    catalog: CatalogConnection,
}

impl Access {
    pub(super) async fn new(config: &Config) -> Self {
        Self {
            metadata: config.metadata.clone(),
            metadata_location: config.metadata_location.clone(),
            catalog: CatalogConnection::connect(&ConnectionConfig::new(
                url::Url::parse(&config.catalog).unwrap(),
            ))
            .await
            .unwrap(),
        }
    }
}

#[async_trait::async_trait]
impl WorkerAccess for Access {
    async fn access(&self, _: JobId) -> Result<TaskAccess, Status> {
        Ok(TaskAccess {
            metadata: self.metadata.clone(),
            store: ObjectStore::open(&StorageProfile::LocalFs, None).unwrap(),
            credentials: None,
        })
    }
}

#[async_trait::async_trait]
impl SchedulingAccess for Access {
    async fn validate(&self, _: &jobs::Job, plan: &PlanDocument) -> Result<Eligibility, Status> {
        let baseline = bergsmith_maintenance::PlanningBaseline::capture(
            &self.metadata,
            Some(&self.metadata_location),
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.baseline(), &baseline);
        Ok(Eligibility::Valid)
    }
    async fn recover_publication(
        &self,
        pool: &PgPool,
        job: &jobs::Job,
        epoch: i64,
    ) -> Result<(), Status> {
        durable_publication::reconcile(
            &mut pool.acquire().await.unwrap(),
            job,
            epoch,
            &self.catalog,
        )
        .await
        .map(|_| ())
        .map_err(|_| Status::unavailable("publication recovery deferred"))
    }
}

pub(super) fn scheduler_config() -> SchedulerConfig {
    SchedulerConfig {
        interval: Duration::from_millis(25),
        retry: RetryPolicy {
            base_millis: 10,
            max_millis: 100,
        },
        ..SchedulerConfig::default()
    }
}

pub(super) async fn service(
    pool: &PgPool,
    epoch: i64,
    config: &Config,
    access: Arc<Access>,
) -> WorkerService {
    let mut conn = pool.acquire().await.unwrap();
    support::worker_token(
        &mut conn,
        config.organization,
        config.principal,
        "native",
        ServiceToken::parse(TOKEN).unwrap().verifier(),
    )
    .await;
    WorkerService::new(
        pool.clone(),
        epoch,
        WorkerConfig {
            heartbeat_interval: Duration::from_secs(1),
            lease: Duration::from_secs(90),
            long_poll_timeout: Duration::from_millis(100),
            queue_capacity: 256,
        },
        access,
    )
    .unwrap()
}

pub(super) async fn certificate(directory: &Path) {
    let status = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-sha256",
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=DNS:localhost",
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-keyout",
        ])
        .arg(directory.join("key.pem"))
        .arg("-out")
        .arg(directory.join("cert.pem"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success());
}

pub(crate) async fn coordinator(path: &Path) {
    let config: Config = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let pool = PgPool::connect(&config.database).await.unwrap();
    let holder = Uuid::now_v7().to_string();
    let epoch = leadership::acquire_leadership(
        &mut pool.acquire().await.unwrap(),
        &holder,
        Duration::from_secs(300),
    )
    .await
    .unwrap()
    .unwrap()
    .epoch;
    let access = Arc::new(Access::new(&config).await);
    let scheduler = Scheduler::new(
        service(&pool, epoch, &config, access.clone()).await,
        access.clone(),
        scheduler_config(),
    )
    .unwrap();
    scheduler.reconcile().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // Rebind the original endpoint after a crash so existing workers reconnect.
    let listener = if let Ok(bytes) = std::fs::read(config.directory.join("endpoint.json")) {
        drop(listener);
        let original: std::net::SocketAddr = serde_json::from_slice(&bytes).unwrap();
        tokio::net::TcpListener::bind(original).await.unwrap()
    } else {
        std::fs::write(
            config.directory.join("endpoint.json"),
            serde_json::to_vec(&address).unwrap(),
        )
        .unwrap();
        listener
    };
    let address = listener.local_addr().unwrap();
    let commands_path = config.directory.join("commands.sock");
    let _ = std::fs::remove_file(&commands_path);
    let commands = UnixDatagram::bind(commands_path).unwrap();
    let (execute, execution_released) =
        tokio::sync::watch::channel(config.directory.join("execution-released").exists());
    let peer = Faults {
        inner: scheduler.worker_service(),
        execution_released,
        directory: config.directory.clone(),
        result_lost: AtomicBool::new(config.directory.join("result-lost.pb").exists()),
        sessions: tokio::sync::Mutex::new(std::collections::BTreeMap::new()),
    };
    let incoming = futures::stream::unfold(listener, |listener| async {
        Some((listener.accept().await.map(|(stream, _)| stream), listener))
    });
    let identity = Identity::from_pem(
        std::fs::read(config.directory.join("cert.pem")).unwrap(),
        std::fs::read(config.directory.join("key.pem")).unwrap(),
    );
    let server = Server::builder()
        .tls_config(worker_tls_config(identity))
        .unwrap()
        .add_service(ClusterServiceServer::new(peer))
        .serve_with_incoming(incoming);
    // Atomic checkpoint: a file's existence never signals a partial payload.
    checkpoint(&config.directory, "ready.json", &serde_json::to_vec(&serde_json::json!({"endpoint": format!("https://localhost:{}", address.port()), "epoch": epoch})).unwrap()).await;
    tokio::select! {
        result = server => result.unwrap(),
        () = scheduler.run(futures::future::pending()) => panic!("scheduler exited"),
        () = control(commands, execute, &pool, &config, epoch, &access.catalog) => panic!("control exited"),
    }
}

async fn control(
    commands: UnixDatagram,
    execute: tokio::sync::watch::Sender<bool>,
    pool: &PgPool,
    config: &Config,
    epoch: i64,
    catalog: &CatalogConnection,
) {
    let mut command = [0; 32];
    loop {
        let size = commands.recv(&mut command).await.unwrap();
        match &command[..size] {
            b"execute" => {
                execute.send(true).unwrap();
            }
            b"publish" => {
                let mut conn = pool.acquire().await.unwrap();
                let job = jobs::job(&mut conn, config.organization, config.job)
                    .await
                    .unwrap();
                // The real publisher lives in this process. The parent kills us
                // after catalog application, before any response is returned.
                durable_publication::publish(&mut conn, &job, epoch, catalog)
                    .await
                    .unwrap();
                panic!("publication response escaped the crash barrier");
            }
            other => panic!("unknown process command {other:?}"),
        }
    }
}

struct Faults {
    inner: WorkerService,
    execution_released: tokio::sync::watch::Receiver<bool>,
    directory: PathBuf,
    result_lost: AtomicBool,
    sessions: tokio::sync::Mutex<std::collections::BTreeMap<String, String>>,
}

#[tonic::async_trait]
impl ClusterService for Faults {
    async fn register_worker(
        &self,
        request: Request<wire::RegisterWorkerRequest>,
    ) -> Result<Response<wire::RegisterWorkerResponse>, Status> {
        let name = request.get_ref().worker_name.clone();
        let result = self.inner.register_worker(request).await?;
        self.sessions
            .lock()
            .await
            .insert(result.get_ref().worker_session_id.clone(), name);
        Ok(result)
    }

    async fn request_task(
        &self,
        request: Request<wire::RequestTaskRequest>,
    ) -> Result<Response<wire::RequestTaskResponse>, Status> {
        let session = request.get_ref().worker_session_id.clone();
        let encoded = request.get_ref().encode_to_vec();
        let result = self.inner.request_task(request).await?;
        if matches!(
            result.get_ref().result,
            Some(wire::request_task_response::Result::Grant(_))
        ) {
            if !self.directory.join("grant-released").exists() {
                if self.directory.join("grant-lost.pb").exists() {
                    assert_eq!(
                        std::fs::read(self.directory.join("poll-lost.pb")).unwrap(),
                        encoded
                    );
                    checkpoint(
                        &self.directory,
                        "grant-replayed.pb",
                        &result.get_ref().encode_to_vec(),
                    )
                    .await;
                    return Ok(result);
                }
                checkpoint(&self.directory, "poll-lost.pb", &encoded).await;
                checkpoint(
                    &self.directory,
                    "grant-lost.pb",
                    &result.get_ref().encode_to_vec(),
                )
                .await;
                return Err(Status::unavailable("injected lost durable grant response"));
            }
            let mut execution_released = self.execution_released.clone();
            if !*execution_released.borrow() {
                let name = self.sessions.lock().await.get(&session).unwrap().clone();
                checkpoint(&self.directory, &format!("started-{name}"), &[]).await;
                // The parent releases only after all three actual workers hold
                // grants. This is an observation barrier, never a timing sleep.
                while !*execution_released.borrow_and_update() {
                    execution_released.changed().await.unwrap();
                }
            }
        }
        Ok(result)
    }

    async fn report_task_result(
        &self,
        request: Request<wire::ReportTaskResultRequest>,
    ) -> Result<Response<wire::ReportTaskResultResponse>, Status> {
        let encoded = request.get_ref().encode_to_vec();
        let result = self.inner.report_task_result(request).await?;
        assert!(matches!(
            wire::ResultOutcome::try_from(result.get_ref().outcome).unwrap(),
            wire::ResultOutcome::Accepted | wire::ResultOutcome::AlreadyAccepted
        ));
        if result.get_ref().job_state
            == i32::from(bergsmith_protocol::common::JobState::ReadyToCommit)
        {
            checkpoint(&self.directory, "execution-complete", &[]).await;
        }
        if !self.result_lost.swap(true, Ordering::AcqRel) {
            checkpoint(&self.directory, "result-lost.pb", &encoded).await;
            // Kill the coordinator before it returns this accepted result.
            return futures::future::pending().await;
        }
        if let Ok(original) = std::fs::read(self.directory.join("result-lost.pb"))
            && original == encoded
        {
            assert_eq!(
                result.get_ref().outcome,
                i32::from(wire::ResultOutcome::AlreadyAccepted)
            );
            checkpoint(&self.directory, "result-replayed.pb", &encoded).await;
        }
        Ok(result)
    }

    async fn heartbeat(
        &self,
        request: Request<wire::HeartbeatRequest>,
    ) -> Result<Response<wire::HeartbeatResponse>, Status> {
        self.inner.heartbeat(request).await
    }
    async fn report_task_failure(
        &self,
        request: Request<wire::ReportTaskFailureRequest>,
    ) -> Result<Response<wire::ReportTaskFailureResponse>, Status> {
        self.inner.report_task_failure(request).await
    }
    async fn get_attempt_status(
        &self,
        request: Request<wire::GetAttemptStatusRequest>,
    ) -> Result<Response<wire::GetAttemptStatusResponse>, Status> {
        self.inner.get_attempt_status(request).await
    }
    async fn drain_worker(
        &self,
        request: Request<wire::DrainWorkerRequest>,
    ) -> Result<Response<wire::DrainWorkerResponse>, Status> {
        self.inner.drain_worker(request).await
    }
}
