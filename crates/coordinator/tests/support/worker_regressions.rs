//! Regressions for the BS-013 correctness and concurrency review.

use super::*;

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_conflicting_receipt_racing_with_acceptance_is_rejected(pool: PgPool) {
    use bergsmith_persistence::publications;
    use sqlx::Connection;
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    let original = run.execute(&grant).await;
    let mut conflicting = original.clone();
    conflicting.metrics.as_mut().unwrap().rows_written += 1;
    let conflicting_digest = receipt_digest(&conflicting).to_hex();
    let mut conn = pool.acquire().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("LOCK TABLE tasks IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .unwrap();
    let report = wire::ReportTaskResultRequest {
        worker_session_id: run.session.clone(),
        receipt_sha256: receipt_digest(&conflicting).as_bytes().to_vec(),
        receipt: Some(conflicting),
    };
    let submission = attempts::ReceiptSubmission {
        attempt_id: grant.attempt_id.parse().unwrap(),
        generation: grant.generation,
        content: serde_json::json!(original.encode_to_vec()),
        content_hash: receipt_digest(&original).to_hex(),
    };
    let objects: Vec<_> = original
        .outputs
        .iter()
        .map(|o| (o.path.clone(), o.file_size_bytes))
        .collect();
    let (response, ()) = tokio::join!(
        run.client.report_task_result(request(&run.token, report)),
        async {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    sqlx::query("SELECT pg_stat_clear_snapshot()")
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                    let blocked: bool = sqlx::query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity
                    WHERE datname = current_database() AND $1 = ANY(pg_blocking_pids(pid))
                    AND query LIKE '%FROM tasks WHERE id =%')",
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
            let accepted =
                publications::accept_outputs(&mut tx, &submission, &grant.output_prefix, &objects)
                    .await
                    .unwrap();
            assert_eq!(accepted.outcome, attempts::AcceptOutcome::Accepted);
            tx.commit().await.unwrap();
        }
    );
    let response = response.unwrap().into_inner();
    let stored: String =
        sqlx::query_scalar("SELECT content_hash FROM receipts WHERE attempt_id = $1")
            .bind(Uuid::parse_str(&grant.attempt_id).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(stored, conflicting_digest);
    assert_eq!(
        response.outcome,
        i32::from(wire::ResultOutcome::InvalidResult)
    );
}

struct BlockedAccess {
    inner: Arc<Access>,
    started: tokio::sync::Notify,
    proceed: tokio::sync::Notify,
}

#[async_trait]
impl WorkerAccess for BlockedAccess {
    async fn access(&self, job: JobId) -> Result<TaskAccess, Status> {
        self.started.notify_one();
        self.proceed.notified().await;
        self.inner.access(job).await
    }
}

async fn blocked_client(
    pool: &PgPool,
    run: &Run,
) -> (
    ClusterServiceClient<Channel>,
    Arc<BlockedAccess>,
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
    let limited = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(1))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let access = Arc::new(BlockedAccess {
        inner: run.access.clone(),
        started: tokio::sync::Notify::new(),
        proceed: tokio::sync::Notify::new(),
    });
    support::worker_token(
        &mut pool.acquire().await.unwrap(),
        run.tenant.organization,
        PrincipalId::from_uuid(principal),
        "native",
        run.token.verifier(),
    )
    .await;
    let service = WorkerService::new(
        limited.clone(),
        epoch,
        WorkerConfig::default(),
        access.clone(),
    )
    .unwrap();
    let (client, server) = tls_client(service).await;
    (client, access, server)
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn slow_credential_refresh_does_not_hold_a_database_connection(pool: PgPool) {
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    let (mut client, access, server) = blocked_client(&pool, &run).await;
    let mut observer = client.clone();
    let report = wire::HeartbeatRequest {
        worker_session_id: run.session.clone(),
        attempts: vec![wire::AttemptLease {
            attempt_id: grant.attempt_id.clone(),
            generation: grant.generation,
            progress: None,
        }],
    };
    let (heartbeat, ()) = tokio::join!(client.heartbeat(request(&run.token, report)), async {
        access.started.notified().await;
        let status = observer
            .get_attempt_status(request(
                &run.token,
                wire::GetAttemptStatusRequest {
                    worker_session_id: run.session.clone(),
                    attempt_id: grant.attempt_id.clone(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            status.result_outcome,
            i32::from(wire::ResultOutcome::Unspecified)
        );
        access.proceed.notify_one();
    });
    assert!(heartbeat.is_ok());
    server.abort();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn reporting_releases_database_access_and_replays_without_catalog_io(pool: PgPool) {
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    let receipt = run.execute(&grant).await;
    let report = wire::ReportTaskResultRequest {
        worker_session_id: run.session.clone(),
        receipt_sha256: receipt_digest(&receipt).as_bytes().to_vec(),
        receipt: Some(receipt),
    };
    let (mut client, access, server) = blocked_client(&pool, &run).await;
    let mut observer = client.clone();
    let (accepted, ()) = tokio::join!(
        client.report_task_result(request(&run.token, report.clone())),
        async {
            access.started.notified().await;
            let status = observer
                .get_attempt_status(request(
                    &run.token,
                    wire::GetAttemptStatusRequest {
                        worker_session_id: run.session.clone(),
                        attempt_id: grant.attempt_id.clone(),
                    },
                ))
                .await
                .unwrap()
                .into_inner();
            assert_eq!(
                status.result_outcome,
                i32::from(wire::ResultOutcome::Unspecified)
            );
            access.proceed.notify_one();
        }
    );
    assert_eq!(
        accepted.unwrap().into_inner().outcome,
        i32::from(wire::ResultOutcome::Accepted)
    );
    // No further permit is supplied: accepted replays must avoid this provider.
    let replay = client
        .report_task_result(request(&run.token, report))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        replay.outcome,
        i32::from(wire::ResultOutcome::AlreadyAccepted)
    );
    server.abort();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_retry_cannot_change_the_tasks_original_resource_envelope(pool: PgPool) {
    let mut run = setup(&pool).await;
    let first = run.grant().await;
    let report = wire::ReportTaskFailureRequest {
        worker_session_id: run.session.clone(),
        attempt_id: first.attempt_id.clone(),
        generation: first.generation,
        failure: Some(common::Failure {
            reason_code: "StorageOperationTimedOut".into(),
            disposition: 0,
            message: String::new(),
        }),
    };
    let failure = run
        .client
        .report_task_failure(request(&run.token, report))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        failure.disposition,
        i32::from(common::FailureDisposition::RetryTask)
    );
    let mut conn = pool.acquire().await.unwrap();
    support::release_task(&mut conn, run.offer.task).await;
    drop(conn);
    run.offer.limits.memory_bytes *= 2;
    assert!(run.service.offer(run.offer.clone()).await.is_err());
    run.offer.limits = first.limits.unwrap();
    let second = run.grant().await;
    assert_eq!(first.job_id, second.job_id);
    assert_eq!(first.task_id, second.task_id);
    assert_eq!(second.generation, first.generation + 1);
    assert_eq!(first.limits, second.limits);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_malformed_output_path_receives_invalid_result(pool: PgPool) {
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    let mut receipt = run.execute(&grant).await;
    receipt.outputs[0].path = "invalid://unsupported/file.parquet".into();
    let response = run
        .client
        .report_task_result(request(
            &run.token,
            wire::ReportTaskResultRequest {
                worker_session_id: run.session.clone(),
                receipt_sha256: receipt_digest(&receipt).as_bytes().to_vec(),
                receipt: Some(receipt),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        response.outcome,
        i32::from(wire::ResultOutcome::InvalidResult)
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_blocked_grant_does_not_block_an_unrelated_poll(pool: PgPool) {
    use sqlx::Connection;
    let mut run = setup(&pool).await;
    run.service.offer(run.offer.clone()).await.unwrap();
    let other = run
        .client
        .register_worker(request(&run.token, registration("drained-worker")))
        .await
        .unwrap()
        .into_inner();
    run.client
        .drain_worker(request(
            &run.token,
            wire::DrainWorkerRequest {
                worker_session_id: other.worker_session_id.clone(),
            },
        ))
        .await
        .unwrap();
    let mut observer = run.client.clone();
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
    let poll = wire::RequestTaskRequest {
        worker_session_id: run.session.clone(),
        request_id: Uuid::now_v7().to_string(),
    };
    let (response, ()) = tokio::join!(run.client.request_task(request(&run.token, poll)), async {
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
        let empty_poll = observer.request_task(request(
            &run.token,
            wire::RequestTaskRequest {
                worker_session_id: other.worker_session_id.clone(),
                request_id: Uuid::now_v7().to_string(),
            },
        ));
        let response = tokio::time::timeout(Duration::from_secs(2), empty_poll)
            .await
            .unwrap()
            .unwrap()
            .into_inner();
        assert!(matches!(
            response.result,
            Some(wire::request_task_response::Result::NoTask(_))
        ));
        tx.commit().await.unwrap();
    });
    assert!(response.is_ok());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn retryable_native_failures_remain_retryable_through_the_worker_rpc(pool: PgPool) {
    let mut run = setup(&pool).await;
    let grant = run.grant().await;
    let failure = bergsmith_maintenance::ExecutorError::Iceberg(
        iceberg::Error::new(iceberg::ErrorKind::Unexpected, "temporary backend failure")
            .with_retryable(true),
    )
    .to_failure();
    assert_eq!(
        failure.disposition,
        i32::from(common::FailureDisposition::RetryTask)
    );
    assert_eq!(failure.reason_code, "TransientNativeExecutionFailed");
    let response = run
        .client
        .report_task_failure(request(
            &run.token,
            wire::ReportTaskFailureRequest {
                worker_session_id: run.session.clone(),
                attempt_id: grant.attempt_id,
                generation: grant.generation,
                failure: Some(failure),
            },
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        response.disposition,
        i32::from(common::FailureDisposition::RetryTask)
    );
    assert_eq!(response.job_state, i32::from(common::JobState::Running));
}
