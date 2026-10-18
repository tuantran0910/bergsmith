//! Native/TLS scheduling and restart acceptance tests (BS-014.T1–T4).
use super::*;
use bergsmith_coordinator::scheduling::{
    Eligibility, Scheduler, SchedulerConfig, SchedulingAccess,
};
use bergsmith_persistence::scheduling as scheduling_ledger;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

#[derive(Default)]
struct Checks {
    outcome: AtomicU8,
    recovered: AtomicUsize,
    started: tokio::sync::Notify,
    proceed: tokio::sync::Notify,
}

#[async_trait]
impl SchedulingAccess for Checks {
    async fn validate(
        &self,
        _: &jobs::Job,
        plan: &bergsmith_maintenance::PlanDocument,
    ) -> Result<Eligibility, Status> {
        assert!(!plan.tasks().is_empty());
        match self.outcome.load(Ordering::Acquire) {
            1 => Ok(Eligibility::ReplanRequired("PlanningBaselineMismatch")),
            2 => Ok(Eligibility::PermanentFailure("PlannedSourceMissing")),
            3 => Err(Status::unavailable("catalog timeout")),
            4 => {
                self.started.notify_one();
                self.proceed.notified().await;
                Ok(Eligibility::Valid)
            }
            _ => Ok(Eligibility::Valid),
        }
    }
    async fn recover_publication(&self, _: &PgPool, _: &jobs::Job) -> Result<(), Status> {
        self.recovered.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

fn scheduler(run: &Run, checks: Arc<Checks>, config: SchedulerConfig) -> Scheduler {
    Scheduler::new(run.service.clone(), checks, config).unwrap()
}

fn granted(response: wire::RequestTaskResponse) -> wire::TaskGrant {
    let Some(wire::request_task_response::Result::Grant(grant)) = response.result else {
        panic!("expected managed grant");
    };
    grant
}

async fn fresh_service(
    pool: &PgPool,
    run: &Run,
    capacity: usize,
    new_epoch: bool,
) -> WorkerService {
    service_with_access(pool, run, capacity, new_epoch, run.access.clone()).await
}

async fn service_with_access(
    pool: &PgPool,
    run: &Run,
    capacity: usize,
    new_epoch: bool,
    access: Arc<dyn WorkerAccess>,
) -> WorkerService {
    let mut conn = pool.acquire().await.unwrap();
    let (principal, mut epoch): (Uuid,i64) = sqlx::query_as("SELECT s.principal_id,l.epoch FROM worker_sessions s CROSS JOIN coordinator_leadership l WHERE s.id=$1")
        .bind(Uuid::parse_str(&run.session).unwrap()).fetch_one(&mut *conn).await.unwrap();
    if new_epoch {
        let holder: String = sqlx::query_scalar("SELECT holder FROM coordinator_leadership")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        bergsmith_persistence::leadership::release_leadership(&mut conn, &holder, epoch)
            .await
            .unwrap();
        epoch = support::leader(&mut conn).await;
    }
    WorkerService::new(
        pool.clone(),
        epoch,
        vec![WorkerVerifier {
            identity: ledger::Identity {
                organization: run.tenant.organization,
                principal: PrincipalId::from_uuid(principal),
                pool: "native".into(),
            },
            verifier: run.token.verifier(),
        }],
        WorkerConfig {
            queue_capacity: capacity,
            long_poll_timeout: Duration::from_millis(20),
            ..WorkerConfig::default()
        },
        access,
    )
    .unwrap()
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn startup_rebuilds_the_same_ready_set_and_retry_timers_from_native_plans(pool: PgPool) {
    let mut run = setup(&pool).await;
    let checks = Arc::new(Checks::default());
    let first = scheduler(&run, checks.clone(), SchedulerConfig::default());
    first
        .register(run.job, std::slice::from_ref(&run.offer))
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
    first.reconcile().await.unwrap();
    let expected = first.worker_service().ready_tasks();
    assert_eq!(expected, vec![run.offer.task]);
    let fresh = fresh_service(&pool, &run, 256, true).await;
    let second = Scheduler::new(fresh, checks.clone(), SchedulerConfig::default()).unwrap();
    assert!(second.worker_service().ready_tasks().is_empty());
    second.reconcile().await.unwrap();
    assert_eq!(second.worker_service().ready_tasks(), expected);
    let (client, server) = tls_client(second.worker_service()).await;
    run.client = client;
    let grant = granted(run.pull(Uuid::now_v7()).await);
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("UPDATE attempts SET granted_at=clock_timestamp()-interval '2 minutes',lease_expires_at=clock_timestamp()-interval '1 minute' WHERE id=$1")
        .bind(Uuid::parse_str(&grant.attempt_id).unwrap()).execute(&mut *conn).await.unwrap();
    drop(conn);
    second.reconcile().await.unwrap();
    let timers = scheduling_ledger::retry_timers(&mut pool.acquire().await.unwrap())
        .await
        .unwrap();
    assert_eq!(timers.len(), 1);
    let third = Scheduler::new(
        fresh_service(&pool, &run, 256, true).await,
        checks,
        SchedulerConfig::default(),
    )
    .unwrap();
    third.reconcile().await.unwrap();
    assert_eq!(
        scheduling_ledger::retry_timers(&mut pool.acquire().await.unwrap())
            .await
            .unwrap(),
        timers
    );
    assert!(third.worker_service().ready_tasks().is_empty());
    sqlx::query(
        "UPDATE tasks SET retry_not_before=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(run.offer.task.as_uuid())
    .execute(&pool)
    .await
    .unwrap();
    third.reconcile().await.unwrap();
    assert_eq!(third.worker_service().ready_tasks(), expected);
    let (client, third_server) = tls_client(third.worker_service()).await;
    run.client = client;
    let next = granted(run.pull(Uuid::now_v7()).await);
    assert_eq!(next.generation, 2);
    assert_ne!(next.attempt_id, grant.attempt_id);
    server.abort();
    third_server.abort();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_catalog_timeout_cannot_release_a_retry_or_create_replacement_execution(pool: PgPool) {
    let mut run = setup(&pool).await;
    let checks = Arc::new(Checks::default());
    let scheduler = scheduler(&run, checks.clone(), SchedulerConfig::default());
    scheduler
        .register(run.job, std::slice::from_ref(&run.offer))
        .await
        .unwrap();
    scheduler.reconcile().await.unwrap();
    let grant = granted(run.pull(Uuid::now_v7()).await);
    run.client
        .report_task_failure(request(
            &run.token,
            wire::ReportTaskFailureRequest {
                worker_session_id: run.session.clone(),
                attempt_id: grant.attempt_id.clone(),
                generation: 1,
                failure: Some(common::Failure {
                    reason_code: "TransientNativeExecutionFailed".into(),
                    message: String::new(),
                    disposition: 0,
                }),
            },
        ))
        .await
        .unwrap();
    sqlx::query(
        "UPDATE tasks SET retry_not_before=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(run.offer.task.as_uuid())
    .execute(&pool)
    .await
    .unwrap();
    let timers = scheduling_ledger::retry_timers(&mut pool.acquire().await.unwrap())
        .await
        .unwrap();
    checks.outcome.store(3, Ordering::Release);
    assert!(scheduler.reconcile().await.is_err());
    assert_eq!(
        scheduling_ledger::retry_timers(&mut pool.acquire().await.unwrap())
            .await
            .unwrap(),
        timers
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    checks.outcome.store(0, Ordering::Release);
    scheduler.reconcile().await.unwrap();
    assert_eq!(
        scheduler.worker_service().ready_tasks(),
        vec![run.offer.task]
    );
    let second = granted(run.pull(Uuid::now_v7()).await);
    assert_eq!(second.generation, 2);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_retry_baseline_conflict_fences_the_job_without_reusing_accepted_output(pool: PgPool) {
    let mut run = setup(&pool).await;
    let checks = Arc::new(Checks::default());
    let scheduler = scheduler(&run, checks.clone(), SchedulerConfig::default());
    scheduler
        .register(run.job, std::slice::from_ref(&run.offer))
        .await
        .unwrap();
    scheduler.reconcile().await.unwrap();
    let grant = granted(run.pull(Uuid::now_v7()).await);
    let receipt = run.execute(&grant).await;
    checks.outcome.store(1, Ordering::Release);
    scheduler.reconcile().await.unwrap();
    assert_eq!(
        jobs::job(
            &mut pool.acquire().await.unwrap(),
            run.tenant.organization,
            run.job
        )
        .await
        .unwrap()
        .state,
        JobState::ReplanRequired
    );
    assert_eq!(
        run.report(receipt).await.outcome,
        i32::from(wire::ResultOutcome::StaleAttempt)
    );
    assert!(scheduler.worker_service().ready_tasks().is_empty());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn permanent_input_failures_and_job_budgets_end_jobs_without_dispatch(pool: PgPool) {
    let run = setup(&pool).await;
    let checks = Arc::new(Checks::default());
    checks.outcome.store(2, Ordering::Release);
    let scheduler = scheduler(&run, checks, SchedulerConfig::default());
    scheduler
        .register(run.job, std::slice::from_ref(&run.offer))
        .await
        .unwrap();
    scheduler.reconcile().await.unwrap();
    assert_eq!(
        jobs::job(
            &mut pool.acquire().await.unwrap(),
            run.tenant.organization,
            run.job
        )
        .await
        .unwrap()
        .state,
        JobState::Failed
    );
    assert!(scheduler.worker_service().ready_tasks().is_empty());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_oversized_immutable_job_is_rejected_instead_of_truncating_its_plan(pool: PgPool) {
    let run = setup(&pool).await;
    let scheduler = scheduler(
        &run,
        Arc::new(Checks::default()),
        SchedulerConfig {
            max_job_input_bytes: 1,
            ..SchedulerConfig::default()
        },
    );
    scheduler
        .register(run.job, std::slice::from_ref(&run.offer))
        .await
        .unwrap();
    scheduler.reconcile().await.unwrap();
    let job = jobs::job(
        &mut pool.acquire().await.unwrap(),
        run.tenant.organization,
        run.job,
    )
    .await
    .unwrap();
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(
        job.state_reason.as_deref(),
        Some("AdmissionJobBudgetExceeded")
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn periodic_reconciliation_runs_without_notifications_and_shutdown_fences_grants(
    pool: PgPool,
) {
    let mut run = setup(&pool).await;
    let scheduler = Arc::new(scheduler(
        &run,
        Arc::new(Checks::default()),
        SchedulerConfig::default(),
    ));
    scheduler
        .register(run.job, std::slice::from_ref(&run.offer))
        .await
        .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let active = scheduler.clone();
    let handle = tokio::spawn(async move {
        active
            .run(async {
                let _ = stopped.await;
            })
            .await;
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while scheduler.worker_service().ready_tasks().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // A cached candidate can appear just before the pass opens dispatch.
    scheduler.reconcile().await.unwrap();
    stop.send(()).unwrap();
    handle.await.unwrap();
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
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn startup_finishes_pending_cancellation_and_keeps_terminal_jobs_closed(pool: PgPool) {
    let run = setup(&pool).await;
    let scheduler = scheduler(
        &run,
        Arc::new(Checks::default()),
        SchedulerConfig::default(),
    );
    scheduler
        .register(run.job, std::slice::from_ref(&run.offer))
        .await
        .unwrap();
    bergsmith_persistence::cancellation::request_cancellation(
        &mut pool.acquire().await.unwrap(),
        &bergsmith_persistence::cancellation::CancelJob {
            job_id: run.job,
            requested_by: None,
            reason: None,
        },
    )
    .await
    .unwrap();
    scheduler.reconcile().await.unwrap();
    assert_eq!(
        jobs::job(
            &mut pool.acquire().await.unwrap(),
            run.tenant.organization,
            run.job
        )
        .await
        .unwrap()
        .state,
        JobState::Cancelled
    );
    scheduler.reconcile().await.unwrap();
    assert!(scheduler.worker_service().ready_tasks().is_empty());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn lost_leadership_closes_dispatch_without_mutating_retry_timers(pool: PgPool) {
    let run = setup(&pool).await;
    let scheduler = scheduler(
        &run,
        Arc::new(Checks::default()),
        SchedulerConfig::default(),
    );
    scheduler
        .register(run.job, std::slice::from_ref(&run.offer))
        .await
        .unwrap();
    scheduler.reconcile().await.unwrap();
    let _new_leader = fresh_service(&pool, &run, 1, true).await;
    assert!(scheduler.reconcile().await.is_err());
    assert!(
        scheduling_ledger::retry_timers(&mut pool.acquire().await.unwrap())
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn bounded_refill_rotates_across_native_tasks_and_restarts_deterministically(pool: PgPool) {
    let fixture = spike::original_spec_with_a_different_default()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let run = setup_with_fixture(&pool, fixture).await;
    let plan = planning::load_plan(&mut pool.acquire().await.unwrap(), run.job)
        .await
        .unwrap()
        .unwrap();
    assert!(plan.tasks().len() > 1);
    let root = Location::parse(run.access.fixture.metadata().location()).unwrap();
    let mut offers = Vec::new();
    for task in plan.tasks() {
        let artifact = plan.artifact_of(task).unwrap();
        let location = root.child(&format!("{}.pb", task.task_id())).unwrap();
        run.access
            .store
            .write(&location, artifact.encode_to_vec().into())
            .await
            .unwrap();
        let mut offer = run.offer.clone();
        offer.task = task.task_id();
        offer.artifact = wire::ArtifactRef {
            uri: location.to_string(),
            sha256: artifact_digest(&artifact).as_bytes().to_vec(),
        };
        offers.push(offer);
    }
    let access = Arc::new(CountedAccess {
        inner: run.access.clone(),
        calls: AtomicUsize::new(0),
        unavailable: std::sync::atomic::AtomicBool::new(false),
    });
    let service = service_with_access(&pool, &run, 1, false, access.clone()).await;
    let scheduler = Scheduler::new(
        service,
        Arc::new(Checks::default()),
        SchedulerConfig::default(),
    )
    .unwrap();
    scheduler.register(run.job, &offers).await.unwrap();
    assert_eq!(access.calls.load(Ordering::Acquire), 1);
    access.unavailable.store(true, Ordering::Release);
    let mut seen = std::collections::BTreeSet::new();
    let mut first = None;
    for _ in 0..plan.tasks().len() {
        scheduler.reconcile().await.unwrap();
        let ready = scheduler.worker_service().ready_tasks();
        assert_eq!(ready.len(), 1);
        first.get_or_insert(ready[0]);
        seen.insert(ready[0]);
    }
    assert_eq!(seen, plan.task_ids().into_iter().collect());
    assert_eq!(access.calls.load(Ordering::Acquire), 1);
    let restarted = Scheduler::new(
        fresh_service(&pool, &run, 1, true).await,
        Arc::new(Checks::default()),
        SchedulerConfig::default(),
    )
    .unwrap();
    restarted.reconcile().await.unwrap();
    assert_eq!(
        restarted.worker_service().ready_tasks(),
        vec![first.unwrap()]
    );
}

struct CountedAccess {
    inner: Arc<Access>,
    calls: AtomicUsize,
    unavailable: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl WorkerAccess for CountedAccess {
    async fn access(&self, job: JobId) -> Result<TaskAccess, Status> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if self.unavailable.load(Ordering::Acquire) {
            return Err(Status::unavailable("credential vending unavailable"));
        }
        self.inner.access(job).await
    }
}

async fn catalog_checks(
    run: &Run,
    metadata: serde_json::Value,
    metadata_location: &str,
    status: usize,
) -> (
    bergsmith_coordinator::worker_access::CatalogWorkerAccess,
    mockito::ServerGuard,
    mockito::Mock,
) {
    use bergsmith_catalog::{CatalogConnection, ConnectionConfig, TableName};
    use bergsmith_coordinator::worker_access::{CatalogWorkerAccess, WorkerTableBinding};
    let mut server = mockito::Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .create_async()
        .await;
    let catalog = CatalogConnection::connect(&ConnectionConfig::new(server.url().parse().unwrap()))
        .await
        .unwrap();
    config.assert_async().await;
    let load = server
        .mock("GET", "/v1/namespaces/db/tables/t")
        .with_status(status)
        .with_body(
            serde_json::json!({"metadata":metadata,"metadata-location":metadata_location})
                .to_string(),
        )
        .create_async()
        .await;
    let access = CatalogWorkerAccess::new(std::collections::BTreeMap::from([(
        run.job,
        WorkerTableBinding {
            catalog: Arc::new(catalog),
            table: TableName {
                namespace: vec!["db".into()],
                name: "t".into(),
            },
            profile: StorageProfile::LocalFs,
        },
    )]));
    (access, server, load)
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn catalog_scheduler_checks_native_inputs_baseline_and_authority_without_rows(pool: PgPool) {
    let fixture = spike::nullable_single_equality_key()
        .unwrap()
        .generate()
        .await
        .unwrap();
    let run = setup_with_fixture(&pool, fixture).await;
    let plan = planning::load_plan(&mut pool.acquire().await.unwrap(), run.job)
        .await
        .unwrap()
        .unwrap();
    let job = jobs::job(
        &mut pool.acquire().await.unwrap(),
        run.tenant.organization,
        run.job,
    )
    .await
    .unwrap();
    let metadata = serde_json::to_value(run.access.fixture.metadata()).unwrap();
    for status in [200, 404, 403, 503] {
        let (access, _server, load) = catalog_checks(
            &run,
            metadata.clone(),
            run.access.fixture.metadata_location(),
            status,
        )
        .await;
        let result = access.validate(&job, &plan).await;
        match status {
            200 => assert_eq!(result.unwrap(), Eligibility::Valid),
            404 => assert_eq!(
                result.unwrap(),
                Eligibility::ReplanRequired("PlanningTableMissing")
            ),
            403 => assert_eq!(
                result.unwrap(),
                Eligibility::PermanentFailure("ExecutionAuthorityRevoked")
            ),
            _ => assert!(result.is_err()),
        }
        load.assert_async().await;
    }
    let mut changed = metadata.clone();
    changed["location"] = serde_json::json!("file:///changed-table");
    let (access, _server, load) =
        catalog_checks(&run, changed, run.access.fixture.metadata_location(), 200).await;
    assert_eq!(
        access.validate(&job, &plan).await.unwrap(),
        Eligibility::ReplanRequired("PlanningBaselineMismatch")
    );
    load.assert_async().await;
    // Corrupt only this test's generated source object under an unchanged baseline.
    let source = Location::parse(plan.tasks()[0].sources()[0].path()).unwrap();
    run.access
        .store
        .write(&source, vec![0].into())
        .await
        .unwrap();
    let (access, _server, load) =
        catalog_checks(&run, metadata, run.access.fixture.metadata_location(), 200).await;
    assert_eq!(
        access.validate(&job, &plan).await.unwrap(),
        Eligibility::PermanentFailure("PlannedSourceIntegrityFailure")
    );
    load.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn missing_planning_metadata_requires_a_new_plan_instead_of_a_retry(pool: PgPool) {
    let run = setup(&pool).await;
    let missing = Location::parse(run.access.fixture.metadata().location())
        .unwrap()
        .child("missing-metadata.json")
        .unwrap();
    let outcome = Planner::new(PlannerConfig {
        min_input_files: 1,
        min_files_removed: 0,
        ..PlannerConfig::default()
    })
    .unwrap()
    .plan(
        PlanInput {
            metadata: run.access.fixture.metadata(),
            metadata_location: Some(&missing.to_string()),
            file_io: run.access.fixture.file_io(),
            selection: &[],
        },
        PlanIds {
            job_id: run.job,
            plan_id: PlanId::generate(),
        },
    )
    .await
    .unwrap();
    let PlanOutcome::Planned(plan) = outcome else {
        panic!("expected native plan");
    };
    let job = jobs::job(
        &mut pool.acquire().await.unwrap(),
        run.tenant.organization,
        run.job,
    )
    .await
    .unwrap();
    let (access, _server, load) = catalog_checks(
        &run,
        serde_json::to_value(run.access.fixture.metadata()).unwrap(),
        &missing.to_string(),
        200,
    )
    .await;
    assert_eq!(
        access.validate(&job, &plan).await.unwrap(),
        Eligibility::ReplanRequired("PlanningMetadataNotRetained")
    );
    load.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn healthy_periodic_validation_does_not_close_previously_validated_dispatch(pool: PgPool) {
    let mut run = setup(&pool).await;
    let checks = Arc::new(Checks::default());
    let scheduler = scheduler(&run, checks.clone(), SchedulerConfig::default());
    scheduler
        .register(run.job, std::slice::from_ref(&run.offer))
        .await
        .unwrap();
    scheduler.reconcile().await.unwrap();
    checks.outcome.store(4, Ordering::Release);
    let (pass, poll) = tokio::join!(scheduler.reconcile(), async {
        checks.started.notified().await;
        let poll = run
            .client
            .request_task(request(
                &run.token,
                wire::RequestTaskRequest {
                    worker_session_id: run.session.clone(),
                    request_id: Uuid::now_v7().to_string(),
                },
            ))
            .await;
        checks.proceed.notify_one();
        poll
    });
    pass.unwrap();
    assert_eq!(granted(poll.unwrap().into_inner()).generation, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn exact_registration_replays_survive_terminal_jobs_and_unreadable_artifacts(pool: PgPool) {
    let mut run = setup(&pool).await;
    let scheduler = scheduler(
        &run,
        Arc::new(Checks::default()),
        SchedulerConfig::default(),
    );
    scheduler
        .register(run.job, std::slice::from_ref(&run.offer))
        .await
        .unwrap();
    scheduler.reconcile().await.unwrap();
    let grant = granted(run.pull(Uuid::now_v7()).await);
    let receipt = run.execute(&grant).await;
    assert_eq!(
        run.report(receipt).await.outcome,
        i32::from(wire::ResultOutcome::Accepted)
    );
    let mut conn = pool.acquire().await.unwrap();
    let epoch: i64 = sqlx::query_scalar("SELECT epoch FROM coordinator_leadership")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    scheduling_ledger::stop(
        &mut conn,
        run.job,
        epoch,
        JobState::Failed,
        "StoppedAfterAcceptance",
    )
    .await
    .unwrap();
    drop(conn);
    // Only this test's generated artifact becomes unreadable; no new grant is possible.
    run.access
        .store
        .write(
            &Location::parse(&run.offer.artifact.uri).unwrap(),
            vec![0].into(),
        )
        .await
        .unwrap();
    scheduler
        .register(run.job, std::slice::from_ref(&run.offer))
        .await
        .unwrap();
    let mut changed = run.offer.clone();
    changed.limits.memory_bytes *= 2;
    assert!(scheduler.register(run.job, &[changed]).await.is_err());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn durable_cancellation_fencing_preserves_cooperative_shutdown_directives(pool: PgPool) {
    let mut run = setup(&pool).await;
    let scheduler = scheduler(
        &run,
        Arc::new(Checks::default()),
        SchedulerConfig::default(),
    );
    scheduler
        .register(run.job, std::slice::from_ref(&run.offer))
        .await
        .unwrap();
    scheduler.reconcile().await.unwrap();
    let grant = granted(run.pull(Uuid::now_v7()).await);
    bergsmith_persistence::cancellation::request_cancellation(
        &mut pool.acquire().await.unwrap(),
        &bergsmith_persistence::cancellation::CancelJob {
            job_id: run.job,
            requested_by: None,
            reason: None,
        },
    )
    .await
    .unwrap();
    let response = run.heartbeat(&grant, grant.generation).await;
    assert!(matches!(
        response.directives[0].directive,
        Some(wire::attempt_directive::Directive::Cancel(_))
    ));
    scheduler.reconcile().await.unwrap();
    let response = run.heartbeat(&grant, grant.generation).await;
    assert!(matches!(
        response.directives[0].directive,
        Some(wire::attempt_directive::Directive::Cancel(_))
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn aborting_the_scheduler_runtime_fences_managed_dispatch(pool: PgPool) {
    let mut run = setup(&pool).await;
    let scheduler = Arc::new(scheduler(
        &run,
        Arc::new(Checks::default()),
        SchedulerConfig::default(),
    ));
    scheduler
        .register(run.job, std::slice::from_ref(&run.offer))
        .await
        .unwrap();
    let active = scheduler.clone();
    let handle = tokio::spawn(async move {
        active.run(std::future::pending()).await;
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while scheduler.worker_service().ready_tasks().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    scheduler.reconcile().await.unwrap();
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    let response = run
        .client
        .request_task(request(
            &run.token,
            wire::RequestTaskRequest {
                worker_session_id: run.session.clone(),
                request_id: Uuid::now_v7().to_string(),
            },
        ))
        .await;
    let error = response.unwrap_err();
    assert_eq!(error.code(), tonic::Code::Unavailable);
    let failure = common::Failure::decode(error.details()).unwrap();
    assert_eq!(failure.reason_code, "SchedulerNotReady");
    assert_eq!(
        failure.disposition,
        i32::from(common::FailureDisposition::RetrySameRequest)
    );
}
