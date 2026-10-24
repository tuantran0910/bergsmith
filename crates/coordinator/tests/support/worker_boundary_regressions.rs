//! Regressions for the BS-013 worker-boundary review: committed renewals,
//! credential refresh timing, typed failures, and grant generation races.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

use super::*;

/// Wraps the fixture access with a controllable outage, credential expiry, and
/// a count of catalog loads.
struct ScriptedAccess {
    inner: Arc<Access>,
    fail: AtomicBool,
    expires_in_secs: AtomicI64,
    loads: AtomicUsize,
}

impl ScriptedAccess {
    fn new(inner: Arc<Access>, expires_in_secs: i64) -> Arc<Self> {
        Arc::new(Self {
            inner,
            fail: AtomicBool::new(false),
            expires_in_secs: AtomicI64::new(expires_in_secs),
            loads: AtomicUsize::new(0),
        })
    }

    fn loads(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl WorkerAccess for ScriptedAccess {
    async fn access(&self, job: JobId) -> Result<TaskAccess, Status> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(Status::unavailable("worker catalog access unavailable"));
        }
        let mut access = self.inner.access(job).await?;
        let expires = chrono::Utc::now()
            + chrono::Duration::seconds(self.expires_in_secs.load(Ordering::SeqCst));
        if let Some(credentials) = access.credentials.as_mut() {
            credentials.expires_at = Some(prost_types::Timestamp {
                seconds: expires.timestamp(),
                nanos: 0,
            });
        }
        Ok(access)
    }
}

/// A second coordinator service over the same ledger and leadership epoch.
async fn scripted_client(
    pool: &PgPool,
    run: &Run,
    access: Arc<ScriptedAccess>,
) -> (
    WorkerService,
    ClusterServiceClient<Channel>,
    tokio::task::JoinHandle<()>,
) {
    let (principal, epoch): (Uuid, i64) = sqlx::query_as(
        "SELECT s.principal_id, l.epoch
        FROM worker_sessions s CROSS JOIN coordinator_leadership l WHERE s.id = $1",
    )
    .bind(Uuid::parse_str(&run.session).unwrap())
    .fetch_one(pool)
    .await
    .unwrap();
    support::worker_token(
        &mut pool.acquire().await.unwrap(),
        run.tenant.organization,
        PrincipalId::from_uuid(principal),
        "native",
        run.token.verifier(),
    )
    .await;
    let service = WorkerService::new(
        pool.clone(),
        epoch,
        WorkerConfig {
            long_poll_timeout: Duration::from_millis(20),
            ..WorkerConfig::default()
        },
        access,
    )
    .unwrap();
    let (client, server) = tls_client(service.clone()).await;
    (service, client, server)
}

fn assert_failure(
    status: &Status,
    code: tonic::Code,
    reason: &str,
    disposition: common::FailureDisposition,
) {
    assert_eq!(status.code(), code, "{status:?}");
    let failure = common::Failure::decode(status.details()).unwrap();
    assert_eq!(failure.reason_code, reason);
    assert_eq!(failure.disposition, i32::from(disposition));
}

fn lease(grant: &wire::TaskGrant, session: &str) -> wire::HeartbeatRequest {
    wire::HeartbeatRequest {
        worker_session_id: session.into(),
        attempts: vec![wire::AttemptLease {
            attempt_id: grant.attempt_id.clone(),
            generation: grant.generation,
            progress: None,
        }],
    }
}

async fn pull_grant(
    client: &mut ClusterServiceClient<Channel>,
    run: &Run,
    poll: wire::RequestTaskRequest,
) -> wire::TaskGrant {
    let response = client
        .request_task(request(&run.token, poll))
        .await
        .unwrap()
        .into_inner();
    let Some(wire::request_task_response::Result::Grant(grant)) = response.result else {
        panic!("expected grant");
    };
    grant
}

fn new_poll(run: &Run) -> wire::RequestTaskRequest {
    wire::RequestTaskRequest {
        worker_session_id: run.session.clone(),
        request_id: Uuid::now_v7().to_string(),
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_failed_credential_refresh_still_returns_the_committed_lease_renewal(pool: PgPool) {
    let run = setup(&pool).await;
    let access = ScriptedAccess::new(run.access.clone(), 30);
    let (service, mut client, server) = scripted_client(&pool, &run, access.clone()).await;
    service.offer(run.offer.clone()).await.unwrap();
    let poll = new_poll(&run);
    let grant = pull_grant(&mut client, &run, poll.clone()).await;
    // The delivered credentials expire within one lease, so a refresh is due.
    access.fail.store(true, Ordering::SeqCst);
    let loads = access.loads();
    let response = client
        .heartbeat(request(&run.token, lease(&grant, &run.session)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(access.loads(), loads + 1);
    let directive = &response.directives[0];
    let Some(wire::attempt_directive::Directive::LeaseExpiresAt(deadline)) = &directive.directive
    else {
        panic!("expected a lease renewal");
    };
    assert!(directive.refreshed_credentials.is_none());
    let attempt = attempts::attempt(
        &mut pool.acquire().await.unwrap(),
        grant.attempt_id.parse().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(attempt.lease_expires_at.timestamp(), deadline.seconds);
    // Grant replay and result verification report the outage with a typed,
    // same-request retry instead of a bare status.
    let error = client
        .request_task(request(&run.token, poll))
        .await
        .unwrap_err();
    assert_failure(
        &error,
        tonic::Code::Unavailable,
        "WorkerTableAccessUnavailable",
        common::FailureDisposition::RetrySameRequest,
    );
    access.fail.store(false, Ordering::SeqCst);
    let receipt = run.execute(&grant).await;
    access.fail.store(true, Ordering::SeqCst);
    let error = client
        .report_task_result(request(
            &run.token,
            wire::ReportTaskResultRequest {
                worker_session_id: run.session.clone(),
                receipt_sha256: receipt_digest(&receipt).as_bytes().to_vec(),
                receipt: Some(receipt),
            },
        ))
        .await
        .unwrap_err();
    assert_failure(
        &error,
        tonic::Code::Unavailable,
        "WorkerTableAccessUnavailable",
        common::FailureDisposition::RetrySameRequest,
    );
    server.abort();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn heartbeats_refresh_credentials_only_when_they_near_expiry(pool: PgPool) {
    let run = setup(&pool).await;
    let access = ScriptedAccess::new(run.access.clone(), 3600);
    let (service, mut client, server) = scripted_client(&pool, &run, access.clone()).await;
    service.offer(run.offer.clone()).await.unwrap();
    let grant = pull_grant(&mut client, &run, new_poll(&run)).await;
    let loads = access.loads();
    for _ in 0..2 {
        let response = client
            .heartbeat(request(&run.token, lease(&grant, &run.session)))
            .await
            .unwrap()
            .into_inner();
        assert!(matches!(
            response.directives[0].directive,
            Some(wire::attempt_directive::Directive::LeaseExpiresAt(_))
        ));
        assert!(response.directives[0].refreshed_credentials.is_none());
    }
    assert_eq!(
        access.loads(),
        loads,
        "fresh credentials need no catalog load"
    );
    // A coordinator without delivery history refreshes, and keeps refreshing
    // while the delivered credentials expire within one lease.
    let (_, mut restarted, restarted_server) = scripted_client(&pool, &run, access.clone()).await;
    access.expires_in_secs.store(30, Ordering::SeqCst);
    for (expected, next_expiry) in [(true, 3600), (true, 3600), (false, 3600)] {
        let response = restarted
            .heartbeat(request(&run.token, lease(&grant, &run.session)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            response.directives[0].refreshed_credentials.is_some(),
            expected
        );
        access.expires_in_secs.store(next_expiry, Ordering::SeqCst);
    }
    assert_eq!(access.loads(), loads + 2);
    restarted_server.abort();
    server.abort();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn rejected_requests_carry_a_typed_reason_and_disposition(pool: PgPool) {
    let mut run = setup(&pool).await;
    let error = run
        .client
        .heartbeat(request(
            &run.token,
            wire::HeartbeatRequest {
                worker_session_id: "not-a-session".into(),
                attempts: vec![],
            },
        ))
        .await
        .unwrap_err();
    assert_failure(
        &error,
        tonic::Code::InvalidArgument,
        "InvalidIdentifier",
        common::FailureDisposition::PermanentFailure,
    );
    let error = run
        .client
        .drain_worker(Request::new(wire::DrainWorkerRequest {
            worker_session_id: run.session.clone(),
        }))
        .await
        .unwrap_err();
    assert_failure(
        &error,
        tonic::Code::Unauthenticated,
        "WorkerTokenInvalid",
        common::FailureDisposition::PermanentFailure,
    );
    let mut old = registration("old-worker");
    old.protocol_revision = 1;
    let error = run
        .client
        .register_worker(request(&run.token, old))
        .await
        .unwrap_err();
    assert_failure(
        &error,
        tonic::Code::FailedPrecondition,
        "WorkerProtocolUnsupported",
        common::FailureDisposition::PermanentFailure,
    );
    let grant = run.grant().await;
    let error = run
        .client
        .report_task_failure(request(
            &run.token,
            wire::ReportTaskFailureRequest {
                worker_session_id: run.session.clone(),
                attempt_id: grant.attempt_id.clone(),
                generation: grant.generation + 1,
                failure: Some(common::Failure {
                    reason_code: "TaskSubprocessCrashed".into(),
                    disposition: 0,
                    message: String::new(),
                }),
            },
        ))
        .await
        .unwrap_err();
    assert_failure(
        &error,
        tonic::Code::FailedPrecondition,
        "StaleAttemptGeneration",
        common::FailureDisposition::PermanentFailure,
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_grant_drafted_before_another_attempt_started_uses_the_locked_generation(pool: PgPool) {
    use sqlx::Connection;
    let mut run = setup(&pool).await;
    run.service.offer(run.offer.clone()).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("SELECT id FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(run.job.as_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
    let task = run.offer.task;
    let poll = new_poll(&run);
    let (response, ()) = tokio::join!(run.client.request_task(request(&run.token, poll)), async {
        // Wait until the grant, drafted from its unlocked task read, waits on the job.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                sqlx::query("SELECT pg_stat_clear_snapshot()")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                let blocked: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM pg_stat_activity
                    WHERE datname = current_database() AND $1 = ANY(pg_blocking_pids(pid)))",
                )
                .bind(blocker)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
                if blocked {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Another attempt started and ended meanwhile, returning the task to Pending.
        sqlx::query("UPDATE tasks SET attempts_started = 1 WHERE id = $1")
            .bind(task.as_uuid())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    });
    let Some(wire::request_task_response::Result::Grant(grant)) =
        response.unwrap().into_inner().result
    else {
        panic!("expected grant");
    };
    let attempt = attempts::attempt(
        &mut pool.acquire().await.unwrap(),
        grant.attempt_id.parse().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(attempt.generation, 2);
    assert_eq!(grant.generation, attempt.generation);
    let renewed = run.heartbeat(&grant, grant.generation).await;
    assert!(matches!(
        renewed.directives[0].directive,
        Some(wire::attempt_directive::Directive::LeaseExpiresAt(_))
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_offer_that_can_never_be_granted_leaves_the_ready_queue(pool: PgPool) {
    let mut run = setup(&pool).await;
    run.service.offer(run.offer.clone()).await.unwrap();
    // The task has used its whole execution budget, so every grant is refused.
    sqlx::query("UPDATE tasks SET attempts_started = 3 WHERE id = $1")
        .bind(run.offer.task.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
    let response = run.pull(Uuid::now_v7()).await;
    assert!(matches!(
        response.result,
        Some(wire::request_task_response::Result::NoTask(_))
    ));
    assert!(run.service.ready_tasks().is_empty());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
