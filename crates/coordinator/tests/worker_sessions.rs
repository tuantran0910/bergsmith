//! Real TLS/tonic calls against PostgreSQL and native Iceberg fixtures.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "integration fixtures fail on setup errors"
)]

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

#[path = "support/worker_regressions.rs"]
mod regressions;

#[path = "support/scheduler_regressions.rs"]
mod scheduling_tests;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bergsmith_common::ids::{AttemptId, JobId, PlanId, PrincipalId, WorkerSessionId};
use bergsmith_common::job::JobState;
use bergsmith_coordinator::{
    planning,
    workers::{
        TaskAccess, TaskOffer, WorkerAccess, WorkerConfig, WorkerService, WorkerVerifier,
        worker_tls_config,
    },
};
use bergsmith_fixtures::{Fixture, spike};
use bergsmith_maintenance::{PlanIds, PlanInput, PlanOutcome, Planner, PlannerConfig};
use bergsmith_persistence::{attempts, jobs, tenancy, workers as ledger};
use bergsmith_protocol::cluster::{
    cluster_service_client::ClusterServiceClient,
    cluster_service_server::{ClusterService, ClusterServiceServer},
};
use bergsmith_protocol::hashing::{artifact_digest, receipt_digest};
use bergsmith_protocol::identity::ServiceToken;
use bergsmith_protocol::{cluster as wire, common};
use bergsmith_storage::{Location, ObjectStore, StorageProfile};
use prost::Message;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity, Server};
use tonic::{Request, Status};
use uuid::Uuid;

struct Access {
    fixture: Fixture,
    store: ObjectStore,
}

#[async_trait]
impl WorkerAccess for Access {
    async fn access(&self, _: JobId) -> Result<TaskAccess, Status> {
        Ok(TaskAccess {
            store: self.store.clone(),
            metadata: self.fixture.metadata().clone(),
            credentials: Some(wire::StorageCredentials {
                prefix: self.fixture.metadata().location().into(),
                properties: [("test-only".into(), "never-persist-this-value".into())].into(),
                expires_at: None,
            }),
        })
    }
}

struct Run {
    service: WorkerService,
    client: ClusterServiceClient<Channel>,
    token: ServiceToken,
    tenant: support::Tenant,
    session: String,
    job: JobId,
    offer: TaskOffer,
    access: Arc<Access>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Run {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn request<T>(token: &ServiceToken, value: T) -> Request<T> {
    let mut request = Request::new(value);
    request.metadata_mut().insert(
        "authorization",
        token.authorization_value().parse().unwrap(),
    );
    request
}

fn ephemeral_tls_identity() -> (Vec<u8>, Vec<u8>) {
    // Generate an ephemeral development key, never a checked-in credential.
    let directory = tempfile::tempdir().unwrap();
    let key = directory.path().join("key.pem");
    let cert = directory.path().join("cert.pem");
    let output = std::process::Command::new("openssl")
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
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output()
        .unwrap();
    assert!(output.status.success());
    (std::fs::read(&cert).unwrap(), std::fs::read(&key).unwrap())
}

async fn tls_client(
    service: WorkerService,
) -> (ClusterServiceClient<Channel>, tokio::task::JoinHandle<()>) {
    let (certificate, key) = tokio::task::spawn_blocking(ephemeral_tls_identity)
        .await
        .unwrap();
    let identity = Identity::from_pem(&certificate, key);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let incoming = futures::stream::unfold(listener, |listener| async {
        Some((listener.accept().await.map(|(stream, _)| stream), listener))
    });
    let server = tokio::spawn(async move {
        Server::builder()
            .tls_config(worker_tls_config(identity))
            .unwrap()
            .add_service(ClusterServiceServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    let channel = Channel::from_shared(format!("https://{address}"))
        .unwrap()
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(certificate))
                .domain_name("localhost"),
        )
        .unwrap()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(5))
        .connect()
        .await
        .unwrap();
    (ClusterServiceClient::new(channel), server)
}

async fn setup(pool: &PgPool) -> Run {
    let fixture = spike::small_files_with_duplicates_and_nulls()
        .unwrap()
        .generate()
        .await
        .unwrap();
    setup_with_fixture(pool, fixture).await
}

async fn setup_with_fixture(pool: &PgPool, fixture: Fixture) -> Run {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = fixture_tenant(&mut conn, &fixture).await;
    let principal = service_principal(&mut conn, tenant.organization).await;
    let job = support::job(&mut conn, &tenant).await;
    let outcome = Planner::new(PlannerConfig {
        min_input_files: 1,
        min_files_removed: 0,
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
    .unwrap();
    let PlanOutcome::Planned(plan) = &outcome else {
        panic!("expected plan");
    };
    let saved = jobs::job(&mut conn, tenant.organization, job)
        .await
        .unwrap();
    planning::record_outcome(&mut conn, &saved, &outcome)
        .await
        .unwrap();
    let task = &plan.tasks()[0];
    let artifact = plan.artifact_of(task).unwrap();
    let store = ObjectStore::open(&StorageProfile::LocalFs, None).unwrap();
    let artifact_location = Location::parse(fixture.metadata().location())
        .unwrap()
        .child("task.pb")
        .unwrap();
    store
        .write(&artifact_location, artifact.encode_to_vec().into())
        .await
        .unwrap();
    let offer = TaskOffer {
        organization: tenant.organization,
        task: task.task_id(),
        pool: "native".into(),
        capability: "native-v1".into(),
        artifact: wire::ArtifactRef {
            uri: artifact_location.to_string(),
            sha256: artifact_digest(&artifact).as_bytes().to_vec(),
        },
        limits: wire::ResourceLimits {
            memory_bytes: 1024,
            cpu_millis: 1000,
            max_duration: Some(prost_types::Duration {
                seconds: 60,
                nanos: 0,
            }),
            max_output_bytes: 1024 * 1024,
        },
    };
    let epoch = support::leader(&mut conn).await;
    let token =
        ServiceToken::parse("bsw1_native_abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMN-0").unwrap();
    let access = Arc::new(Access { fixture, store });
    let service = configured_service(
        pool,
        epoch,
        principal,
        tenant.organization,
        &token,
        access.clone(),
    );
    let (mut client, server) = tls_client(service.clone()).await;
    let registration = client
        .register_worker(request(&token, registration("worker-a")))
        .await
        .unwrap()
        .into_inner();
    Run {
        service,
        client,
        token,
        tenant,
        session: registration.worker_session_id,
        job,
        offer,
        access,
        server,
    }
}

async fn service_principal(
    conn: &mut sqlx::PgConnection,
    organization: bergsmith_common::ids::OrganizationId,
) -> PrincipalId {
    let principal = PrincipalId::generate();
    tenancy::create_principal(
        conn,
        &tenancy::NewPrincipal {
            id: principal,
            organization_id: organization,
            kind: tenancy::PrincipalKind::Service,
            issuer: "bergsmith:test".into(),
            subject: principal.to_string(),
            display_name: "workers".into(),
        },
    )
    .await
    .unwrap();
    principal
}

async fn fixture_tenant(conn: &mut sqlx::PgConnection, fixture: &Fixture) -> support::Tenant {
    let mut tenant = support::tenant(conn, "acme").await;
    tenant.table = bergsmith_persistence::tables::resolve_table(
        conn,
        tenant.organization,
        &support::alias(tenant.connection, "native"),
        fixture.metadata().uuid(),
    )
    .await
    .unwrap()
    .id;
    tenant
}

fn configured_service(
    pool: &PgPool,
    epoch: i64,
    principal: PrincipalId,
    organization: bergsmith_common::ids::OrganizationId,
    token: &ServiceToken,
    access: Arc<Access>,
) -> WorkerService {
    WorkerService::new(
        pool.clone(),
        epoch,
        vec![WorkerVerifier {
            identity: ledger::Identity {
                organization,
                principal,
                pool: "native".into(),
            },
            verifier: token.verifier(),
        }],
        WorkerConfig {
            long_poll_timeout: Duration::from_millis(20),
            ..WorkerConfig::default()
        },
        access,
    )
    .unwrap()
}

fn registration(name: &str) -> wire::RegisterWorkerRequest {
    wire::RegisterWorkerRequest {
        worker_name: name.into(),
        pool: "native".into(),
        protocol_revision: 2,
        capabilities: vec!["native-v1".into()],
        capacity: Some(wire::Capacity {
            max_concurrent_tasks: 1,
            memory_bytes: 4096,
            cpu_millis: 2000,
        }),
        build_version: "test".into(),
    }
}

impl Run {
    async fn execute(&self, grant: &wire::TaskGrant) -> bergsmith_protocol::artifact::TaskReceipt {
        use bergsmith_maintenance::{
            AttemptContext, ExecutionInput, ExecutionLimits, ExecutionRequest, Executor,
            NativeExecutor,
        };
        let bytes = self
            .access
            .store
            .read(&Location::parse(&self.offer.artifact.uri).unwrap())
            .await
            .unwrap();
        let artifact = bergsmith_protocol::artifact::TaskArtifact::decode(bytes).unwrap();
        let prefix = Location::parse(&grant.output_prefix).unwrap();
        NativeExecutor
            .execute(ExecutionRequest {
                input: ExecutionInput {
                    artifact: &artifact,
                    artifact_sha256: artifact_digest(&artifact),
                    metadata: self.access.fixture.metadata(),
                    metadata_location: self.access.fixture.metadata_location(),
                    file_io: self.access.fixture.file_io(),
                },
                attempt: AttemptContext {
                    attempt_id: grant.attempt_id.parse().unwrap(),
                    generation: grant.generation,
                },
                output_prefix: &prefix,
                limits: ExecutionLimits::default(),
            })
            .await
            .unwrap()
            .receipt()
            .clone()
    }

    async fn report(
        &mut self,
        receipt: bergsmith_protocol::artifact::TaskReceipt,
    ) -> wire::ReportTaskResultResponse {
        let report = wire::ReportTaskResultRequest {
            worker_session_id: self.session.clone(),
            receipt_sha256: receipt_digest(&receipt).as_bytes().to_vec(),
            receipt: Some(receipt),
        };
        self.client
            .report_task_result(request(&self.token, report))
            .await
            .unwrap()
            .into_inner()
    }

    async fn pull(&mut self, request_id: Uuid) -> wire::RequestTaskResponse {
        self.client
            .request_task(request(
                &self.token,
                wire::RequestTaskRequest {
                    worker_session_id: self.session.clone(),
                    request_id: request_id.to_string(),
                },
            ))
            .await
            .unwrap()
            .into_inner()
    }

    async fn grant(&mut self) -> wire::TaskGrant {
        self.service.offer(self.offer.clone()).await.unwrap();
        let response = self.pull(Uuid::now_v7()).await;
        let Some(wire::request_task_response::Result::Grant(grant)) = response.result else {
            panic!("expected grant");
        };
        grant
    }

    async fn heartbeat(
        &mut self,
        grant: &wire::TaskGrant,
        generation: i32,
    ) -> wire::HeartbeatResponse {
        self.client
            .heartbeat(request(
                &self.token,
                wire::HeartbeatRequest {
                    worker_session_id: self.session.clone(),
                    attempts: vec![wire::AttemptLease {
                        attempt_id: grant.attempt_id.clone(),
                        generation,
                        progress: None,
                    }],
                },
            ))
            .await
            .unwrap()
            .into_inner()
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn lost_poll_response_recovers_one_durable_grant_without_persisting_credentials(
    pool: PgPool,
) {
    let mut run = setup(&pool).await;
    run.service.offer(run.offer.clone()).await.unwrap();
    let poll = Uuid::now_v7();
    let first = run.pull(poll).await;
    let replay = run.pull(poll).await;
    assert_eq!(first, replay);
    let mut conn = pool.acquire().await.unwrap();
    let stored = ledger::poll(&mut conn, run.session.parse().unwrap(), poll)
        .await
        .unwrap()
        .unwrap();
    let grant = wire::TaskGrant::decode(stored.grant_bytes.unwrap().as_slice()).unwrap();
    assert!(grant.credentials.is_none());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let prefixes: i64 = sqlx::query_scalar("SELECT count(*) FROM attempt_output_prefixes")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(prefixes, 1);
    assert_eq!(
        jobs::job(&mut conn, run.tenant.organization, run.job)
            .await
            .unwrap()
            .state,
        JobState::Running
    );
    // A new service instance has no queue or transient assignment state.
    let (principal, epoch): (Uuid, i64) = sqlx::query_as(
        "SELECT s.principal_id, l.epoch
        FROM worker_sessions s CROSS JOIN coordinator_leadership l WHERE s.id = $1",
    )
    .bind(Uuid::parse_str(&run.session).unwrap())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    let restarted = configured_service(
        &pool,
        epoch,
        PrincipalId::from_uuid(principal),
        run.tenant.organization,
        &run.token,
        run.access.clone(),
    );
    let (mut recovered, server) = tls_client(restarted).await;
    let result = recovered
        .request_task(request(
            &run.token,
            wire::RequestTaskRequest {
                worker_session_id: run.session.clone(),
                request_id: poll.to_string(),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(result, first);
    server.abort();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn concurrent_duplicate_polls_create_only_one_attempt(pool: PgPool) {
    let mut run = setup(&pool).await;
    run.service.offer(run.offer.clone()).await.unwrap();
    let mut other = run.client.clone();
    let poll = wire::RequestTaskRequest {
        worker_session_id: run.session.clone(),
        request_id: Uuid::now_v7().to_string(),
    };
    let (a, b) = tokio::join!(
        run.client.request_task(request(&run.token, poll.clone())),
        other.request_task(request(&run.token, poll))
    );
    assert_eq!(a.unwrap().into_inner(), b.unwrap().into_inner());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn empty_poll_stays_empty_after_work_arrives(pool: PgPool) {
    let mut run = setup(&pool).await;
    let poll = Uuid::now_v7();
    let empty = run.pull(poll).await;
    assert!(matches!(
        empty.result,
        Some(wire::request_task_response::Result::NoTask(_))
    ));
    run.service.offer(run.offer.clone()).await.unwrap();
    assert_eq!(run.pull(poll).await, empty);
    assert!(matches!(
        run.pull(Uuid::now_v7()).await.result,
        Some(wire::request_task_response::Result::Grant(_))
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn heartbeats_renew_only_the_live_generation_and_refresh_credentials(pool: PgPool) {
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    let stale = run.heartbeat(&grant, grant.generation + 1).await;
    assert!(matches!(
        stale.directives[0].directive,
        Some(wire::attempt_directive::Directive::Fenced(_))
    ));
    let renewed = run.heartbeat(&grant, grant.generation).await;
    assert!(matches!(
        renewed.directives[0].directive,
        Some(wire::attempt_directive::Directive::LeaseExpiresAt(_))
    ));
    assert!(renewed.directives[0].refreshed_credentials.is_some());
    let mut conn = pool.acquire().await.unwrap();
    let attempt = attempts::attempt(&mut conn, grant.attempt_id.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(attempt.state, bergsmith_common::task::AttemptState::Running);
    support::expire_lease(&mut conn, attempt.id).await;
    drop(conn);
    assert!(matches!(
        run.heartbeat(&grant, grant.generation).await.directives[0].directive,
        Some(wire::attempt_directive::Directive::Fenced(_))
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn drain_stops_grants_without_stopping_existing_work(pool: PgPool) {
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    for _ in 0..2 {
        run.client
            .drain_worker(request(
                &run.token,
                wire::DrainWorkerRequest {
                    worker_session_id: run.session.clone(),
                },
            ))
            .await
            .unwrap();
    }
    let heartbeat = run.heartbeat(&grant, grant.generation).await;
    assert!(heartbeat.drain);
    assert!(matches!(
        heartbeat.directives[0].directive,
        Some(wire::attempt_directive::Directive::LeaseExpiresAt(_))
    ));
    assert!(matches!(
        run.pull(Uuid::now_v7()).await.result,
        Some(wire::request_task_response::Result::NoTask(_))
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn expired_assignment_poll_returns_disposition_instead_of_another_task(pool: PgPool) {
    let mut run = setup(&pool).await;
    run.service.offer(run.offer.clone()).await.unwrap();
    let poll = Uuid::now_v7();
    let Some(wire::request_task_response::Result::Grant(grant)) = run.pull(poll).await.result
    else {
        panic!("grant");
    };
    let mut conn = pool.acquire().await.unwrap();
    support::expire_lease(&mut conn, grant.attempt_id.parse().unwrap()).await;
    drop(conn);
    let Some(wire::request_task_response::Result::Disposition(status)) =
        run.pull(poll).await.result
    else {
        panic!("disposition");
    };
    assert_eq!(
        status.result_outcome,
        i32::from(wire::ResultOutcome::StaleAttempt)
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn authentication_pool_protocol_and_session_binding_are_enforced(pool: PgPool) {
    let mut run = setup(&pool).await;
    let error = run
        .client
        .register_worker(Request::new(registration("missing-token")))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Unauthenticated);
    let mut wrong = registration("wrong-pool");
    wrong.pool = "other".into();
    assert_eq!(
        run.client
            .register_worker(request(&run.token, wrong))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::PermissionDenied
    );
    let mut old = registration("old");
    old.protocol_revision = 0;
    assert_eq!(
        run.client
            .register_worker(request(&run.token, old))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    let mut previous = registration("revision-one");
    previous.protocol_revision = 1;
    assert_eq!(
        run.client
            .register_worker(request(&run.token, previous))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert!(
        run.client
            .request_task(request(
                &run.token,
                wire::RequestTaskRequest {
                    worker_session_id: WorkerSessionId::generate().to_string(),
                    request_id: Uuid::now_v7().to_string(),
                }
            ))
            .await
            .is_err()
    );
    // Direct/plaintext calls cannot manufacture transport authentication.
    assert_eq!(
        run.service
            .register_worker(request(&run.token, registration("plaintext")))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn registering_the_same_worker_name_fences_its_previous_session(pool: PgPool) {
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    let replacement = run
        .client
        .register_worker(request(&run.token, registration("worker-a")))
        .await
        .unwrap()
        .into_inner();
    assert_ne!(replacement.worker_session_id, run.session);
    assert!(
        run.client
            .heartbeat(request(
                &run.token,
                wire::HeartbeatRequest {
                    worker_session_id: run.session.clone(),
                    attempts: vec![]
                }
            ))
            .await
            .is_err()
    );
    let status = run
        .client
        .get_attempt_status(request(
            &run.token,
            wire::GetAttemptStatusRequest {
                worker_session_id: run.session.clone(),
                attempt_id: grant.attempt_id,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        status.result_outcome,
        i32::from(wire::ResultOutcome::StaleAttempt)
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn failure_reports_are_classified_once_and_do_not_persist_worker_messages(pool: PgPool) {
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    let report = wire::ReportTaskFailureRequest {
        worker_session_id: run.session.clone(),
        attempt_id: grant.attempt_id.clone(),
        generation: grant.generation,
        failure: Some(common::Failure {
            reason_code: "OutputBytesLimitExceeded".into(),
            disposition: common::FailureDisposition::RetryTask.into(),
            message: "untrusted secret text".into(),
        }),
    };
    let first = run
        .client
        .report_task_failure(request(&run.token, report.clone()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        first.disposition,
        i32::from(common::FailureDisposition::PermanentFailure)
    );
    assert_eq!(first.job_state, i32::from(common::JobState::Failed));
    assert_eq!(
        first,
        run.client
            .report_task_failure(request(&run.token, report))
            .await
            .unwrap()
            .into_inner()
    );
    let row: String = sqlx::query_scalar("SELECT row_to_json(f)::text FROM attempt_failures f")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!row.contains("untrusted secret text"));
    let mut conn = pool.acquire().await.unwrap();
    assert!(
        attempts::attempt(&mut conn, grant.attempt_id.parse::<AttemptId>().unwrap())
            .await
            .unwrap()
            .state
            .is_terminal()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn native_receipt_is_accepted_once_and_replayed_after_session_expiry(pool: PgPool) {
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    let receipt = run.execute(&grant).await;
    let accepted = run.report(receipt.clone()).await;
    assert_eq!(accepted.outcome, i32::from(wire::ResultOutcome::Accepted));
    assert_eq!(
        accepted.job_state,
        i32::from(common::JobState::ReadyToCommit)
    );
    sqlx::query(
        "UPDATE worker_sessions SET lease_expires_at = clock_timestamp() - interval '1 second'
        WHERE id = $1",
    )
    .bind(Uuid::parse_str(&run.session).unwrap())
    .execute(&pool)
    .await
    .unwrap();
    let replay = run.report(receipt.clone()).await;
    assert_eq!(
        replay.outcome,
        i32::from(wire::ResultOutcome::AlreadyAccepted)
    );
    let mut conflicting = receipt;
    conflicting.metrics.as_mut().unwrap().rows_written += 1;
    assert_eq!(
        run.report(conflicting).await.outcome,
        i32::from(wire::ResultOutcome::InvalidResult)
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let status = run
        .client
        .get_attempt_status(request(
            &run.token,
            wire::GetAttemptStatusRequest {
                worker_session_id: run.session.clone(),
                attempt_id: grant.attempt_id,
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        status.result_outcome,
        i32::from(wire::ResultOutcome::AlreadyAccepted)
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn result_acknowledgments_distinguish_invalid_stale_and_cancelled(pool: PgPool) {
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    let receipt = run.execute(&grant).await;
    let mut invalid = receipt.clone();
    invalid.metrics.as_mut().unwrap().rows_written += 1;
    assert_eq!(
        run.report(invalid).await.outcome,
        i32::from(wire::ResultOutcome::InvalidResult)
    );
    let mut stale = receipt.clone();
    stale.generation += 1;
    assert_eq!(
        run.report(stale).await.outcome,
        i32::from(wire::ResultOutcome::StaleAttempt)
    );
    let mut conn = pool.acquire().await.unwrap();
    support::force_job_state(&mut conn, run.job, JobState::Cancelling).await;
    drop(conn);
    assert_eq!(
        run.report(receipt).await.outcome,
        i32::from(wire::ResultOutcome::Cancelled)
    );
    assert!(matches!(
        run.heartbeat(&grant, grant.generation).await.directives[0].directive,
        Some(wire::attempt_directive::Directive::Cancel(_))
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn foreign_session_cannot_read_renew_or_report_another_sessions_attempt(pool: PgPool) {
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    let receipt = run.execute(&grant).await;
    let other = run
        .client
        .register_worker(request(&run.token, registration("worker-b")))
        .await
        .unwrap()
        .into_inner();
    run.session = other.worker_session_id;
    assert!(
        run.client
            .get_attempt_status(request(
                &run.token,
                wire::GetAttemptStatusRequest {
                    worker_session_id: run.session.clone(),
                    attempt_id: grant.attempt_id.clone(),
                }
            ))
            .await
            .is_err()
    );
    assert!(
        run.client
            .heartbeat(request(
                &run.token,
                wire::HeartbeatRequest {
                    worker_session_id: run.session.clone(),
                    attempts: vec![wire::AttemptLease {
                        attempt_id: grant.attempt_id,
                        generation: grant.generation,
                        progress: None
                    }]
                }
            ))
            .await
            .is_err()
    );
    assert!(
        run.client
            .report_task_result(request(
                &run.token,
                wire::ReportTaskResultRequest {
                    worker_session_id: run.session.clone(),
                    receipt_sha256: receipt_digest(&receipt).as_bytes().to_vec(),
                    receipt: Some(receipt),
                }
            ))
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_expired_unaccepted_receipt_never_enters_the_ledger(pool: PgPool) {
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    let receipt = run.execute(&grant).await;
    let mut conn = pool.acquire().await.unwrap();
    support::expire_lease(&mut conn, grant.attempt_id.parse().unwrap()).await;
    drop(conn);
    assert_eq!(
        run.report(receipt).await.outcome,
        i32::from(wire::ResultOutcome::StaleAttempt)
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn zero_output_native_receipt_completes_through_the_worker_rpc(pool: PgPool) {
    let fixture = spike::fully_deleted_group()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let mut run = setup_with_fixture(&pool, fixture).await;
    let grant = run.grant().await;
    let receipt = run.execute(&grant).await;
    assert!(receipt.outputs.is_empty());
    let accepted = run.report(receipt).await;
    assert_eq!(accepted.outcome, i32::from(wire::ResultOutcome::Accepted));
    assert_eq!(
        accepted.job_state,
        i32::from(common::JobState::ReadyToCommit)
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn lost_leadership_prevents_grants_and_carries_a_same_request_retry_disposition(
    pool: PgPool,
) {
    let mut run = setup(&pool).await;
    run.service.offer(run.offer.clone()).await.unwrap();
    sqlx::query("UPDATE coordinator_leadership SET lease_expires_at = acquired_at")
        .execute(&pool)
        .await
        .unwrap();
    let error = run
        .client
        .request_task(request(
            &run.token,
            wire::RequestTaskRequest {
                worker_session_id: run.session.clone(),
                request_id: Uuid::now_v7().to_string(),
            },
        ))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Unavailable);
    let failure = common::Failure::decode(error.details()).unwrap();
    assert_eq!(failure.reason_code, "CoordinatorLeadershipLost");
    assert_eq!(
        failure.disposition,
        i32::from(common::FailureDisposition::RetrySameRequest)
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn another_service_principal_in_the_same_pool_cannot_use_an_existing_session(pool: PgPool) {
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    let mut conn = pool.acquire().await.unwrap();
    let principal = service_principal(&mut conn, run.tenant.organization).await;
    let epoch: i64 = sqlx::query_scalar("SELECT epoch FROM coordinator_leadership")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let other =
        ServiceToken::parse("bsw1_native_Abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMN-0").unwrap();
    let service = configured_service(
        &pool,
        epoch,
        principal,
        run.tenant.organization,
        &other,
        run.access.clone(),
    );
    let (mut client, server) = tls_client(service).await;
    assert!(
        client
            .get_attempt_status(request(
                &other,
                wire::GetAttemptStatusRequest {
                    worker_session_id: run.session.clone(),
                    attempt_id: grant.attempt_id,
                }
            ))
            .await
            .is_err()
    );
    server.abort();
}
