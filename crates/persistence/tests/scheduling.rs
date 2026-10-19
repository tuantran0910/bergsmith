//! Atomic admission, immutable dispatch, and database-time retry recovery.
#![expect(clippy::unwrap_used, reason = "integration setup and assertions")]

mod support;

use bergsmith_common::ids::{
    AttemptId, JobId, OrganizationId, PrincipalId, TaskId, WorkerSessionId,
};
use bergsmith_common::job::JobState;
use bergsmith_persistence::{
    attempts, cancellation, jobs, plans, scheduling as scheduler, workers,
};
use sqlx::{PgConnection, PgPool};
use std::time::Duration;
use uuid::Uuid;

fn spec(task: TaskId) -> scheduler::DispatchSpec {
    scheduler::DispatchSpec {
        task_id: *task.as_uuid(),
        pool: "test".into(),
        capability: "native-v1".into(),
        artifact_uri: "file:///tmp/task.pb".into(),
        artifact_sha256: vec![1; 32],
        envelope_bytes: vec![1],
        memory_bytes: 100,
        cpu_millis: 100,
    }
}

async fn configured(
    conn: &mut PgConnection,
    tenant: &support::Tenant,
    count: usize,
) -> (JobId, Vec<TaskId>) {
    let job = support::job(conn, tenant).await;
    let tasks = support::plan(conn, job, count).await;
    support::force_job_state(conn, job, JobState::Queued).await;
    scheduler::register_dispatch(
        conn,
        job,
        &tasks.iter().copied().map(spec).collect::<Vec<_>>(),
        scheduler::RetryPolicy::default(),
    )
    .await
    .unwrap();
    (job, tasks)
}

async fn grant(
    conn: &mut PgConnection,
    task: TaskId,
    epoch: i64,
    limits: scheduler::AdmissionLimits,
) -> Result<workers::Poll, bergsmith_persistence::PersistenceError> {
    let session = support::worker_session(conn, task).await;
    let (organization, principal): (Uuid, Uuid) =
        sqlx::query_as("SELECT organization_id,principal_id FROM worker_sessions WHERE id=$1")
            .bind(session.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    let identity = workers::Identity {
        organization: OrganizationId::from_uuid(organization),
        principal: PrincipalId::from_uuid(principal),
        pool: "test".into(),
    };
    let attempt_id = AttemptId::generate();
    workers::grant_admitted(
        conn,
        &identity,
        &attempts::NewGrant {
            attempt_id,
            task_id: task,
            worker_session_id: session,
            lease: Duration::from_secs(90),
        },
        Uuid::now_v7(),
        epoch,
        &workers::GrantPayload {
            grant_bytes: b"safe",
            output_prefix: &format!("file:///tmp/{attempt_id}"),
            envelope_bytes: &[1],
        },
        limits,
    )
    .await
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn dispatch_configuration_is_complete_atomic_and_immutable(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (job, tasks) = configured(&mut conn, &tenant, 2).await;
    let original: Vec<_> = tasks.iter().copied().map(spec).collect();
    scheduler::register_dispatch(&mut conn, job, &original, scheduler::RetryPolicy::default())
        .await
        .unwrap();
    assert!(
        scheduler::register_dispatch(
            &mut conn,
            job,
            &original[..1],
            scheduler::RetryPolicy::default()
        )
        .await
        .is_err()
    );
    let mut changed = original.clone();
    changed[1].artifact_uri = "file:///tmp/changed.pb".into();
    assert!(
        scheduler::register_dispatch(&mut conn, job, &changed, scheduler::RetryPolicy::default())
            .await
            .is_err()
    );
    changed = original.clone();
    changed[1].pool = "other".into();
    assert!(
        scheduler::register_dispatch(&mut conn, job, &changed, scheduler::RetryPolicy::default())
            .await
            .is_err()
    );
    assert!(
        scheduler::register_dispatch(
            &mut conn,
            job,
            &original,
            scheduler::RetryPolicy {
                base_millis: 2,
                max_millis: 3
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        scheduler::dispatch_spec(&mut conn, tasks[1]).await.unwrap(),
        Some(original[1].clone())
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn aliases_share_table_ownership_until_the_owner_is_terminal(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (first, _) = configured(&mut conn, &tenant, 1).await;
    let (second, _) = configured(&mut conn, &tenant, 1).await;
    let epoch = support::leader(&mut conn).await;
    let limits = scheduler::AdmissionLimits::default();
    assert!(
        scheduler::admit(&mut conn, first, epoch, limits)
            .await
            .unwrap()
    );
    assert!(
        scheduler::admit(&mut conn, first, epoch, limits)
            .await
            .unwrap()
    );
    assert!(
        !scheduler::admit(&mut conn, second, epoch, limits)
            .await
            .unwrap()
    );
    stop_current(
        &mut conn,
        first,
        epoch,
        JobState::ReplanRequired,
        "PlanningBaselineMismatch",
    )
    .await
    .unwrap();
    assert!(
        scheduler::admit(&mut conn, second, epoch, limits)
            .await
            .unwrap()
    );
    assert!(
        scheduler::admit(&mut conn, first, epoch, limits)
            .await
            .is_err()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn concurrent_admissions_cannot_exceed_global_or_pool_budgets(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let first_tenant = support::tenant(&mut conn, "acme").await;
    let second_tenant = support::tenant(&mut conn, "other").await;
    let (first, _) = configured(&mut conn, &first_tenant, 1).await;
    let (second, _) = configured(&mut conn, &second_tenant, 1).await;
    let epoch = support::leader(&mut conn).await;
    let mut other = pool.acquire().await.unwrap();
    let limits = scheduler::AdmissionLimits {
        pool_jobs: 1,
        ..scheduler::AdmissionLimits::default()
    };
    let (a, b) = tokio::join!(
        scheduler::admit(&mut conn, first, epoch, limits),
        scheduler::admit(&mut other, second, epoch, limits)
    );
    assert_ne!(a.unwrap(), b.unwrap());
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM execution_admissions WHERE NOT released")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn concurrent_grants_reserve_memory_cpu_and_slots_atomically(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (job, tasks) = configured(&mut conn, &tenant, 2).await;
    let epoch = support::leader(&mut conn).await;
    let limits = scheduler::AdmissionLimits {
        memory_bytes: 100,
        cpu_millis: 100,
        attempts: 1,
        ..scheduler::AdmissionLimits::default()
    };
    assert!(
        scheduler::admit(&mut conn, job, epoch, limits)
            .await
            .unwrap()
    );
    let mut other = pool.acquire().await.unwrap();
    let (a, b) = tokio::join!(
        grant(&mut conn, tasks[0], epoch, limits),
        grant(&mut other, tasks[1], epoch, limits)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn expiry_preserves_retry_deadlines_and_exhausts_exactly_three_attempts(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (job, tasks) = configured(&mut conn, &tenant, 1).await;
    let epoch = support::leader(&mut conn).await;
    let limits = scheduler::AdmissionLimits::default();
    scheduler::admit(&mut conn, job, epoch, limits)
        .await
        .unwrap();
    for generation in 1..=3 {
        let poll = grant(&mut conn, tasks[0], epoch, limits).await.unwrap();
        let id = poll.attempt_id.unwrap();
        sqlx::query("UPDATE attempts SET granted_at=clock_timestamp()-interval '2 minutes',lease_expires_at=clock_timestamp()-interval '1 minute' WHERE id=$1").bind(id).execute(&mut *conn).await.unwrap();
        scheduler::reconcile_execution(&mut conn, job, epoch)
            .await
            .unwrap();
        let attempt = attempts::attempt(&mut conn, AttemptId::from_uuid(id))
            .await
            .unwrap();
        assert_eq!(attempt.generation, generation);
        assert_eq!(attempt.state, bergsmith_common::task::AttemptState::Expired);
        if generation < 3 {
            let timers = scheduler::retry_timers(&mut conn).await.unwrap();
            assert_eq!(timers.len(), 1);
            scheduler::reconcile_execution(&mut conn, job, epoch)
                .await
                .unwrap();
            assert_eq!(scheduler::retry_timers(&mut conn).await.unwrap(), timers);
            sqlx::query("UPDATE tasks SET retry_not_before=clock_timestamp()-interval '1 second' WHERE id=$1").bind(tasks[0].as_uuid()).execute(&mut *conn).await.unwrap();
            scheduler::reconcile_execution(&mut conn, job, epoch)
                .await
                .unwrap();
            assert_eq!(
                plans::task(&mut conn, tasks[0]).await.unwrap().state,
                bergsmith_common::task::TaskState::Pending
            );
        }
    }
    assert_eq!(
        jobs::job(&mut conn, tenant.organization, job)
            .await
            .unwrap()
            .state,
        JobState::Failed
    );
    assert!(scheduler::ready(&mut conn, 100).await.unwrap().is_empty());
    assert!(scheduler::retry_timers(&mut conn).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_fences_unfinished_work_and_preserves_accepted_receipts(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (job, tasks) = configured(&mut conn, &tenant, 3).await;
    let epoch = support::leader(&mut conn).await;
    let limits = scheduler::AdmissionLimits::default();
    scheduler::admit(&mut conn, job, epoch, limits)
        .await
        .unwrap();
    let accepted = grant(&mut conn, tasks[0], epoch, limits)
        .await
        .unwrap()
        .attempt_id
        .unwrap();
    attempts::accept_receipt(
        &mut conn,
        &attempts::ReceiptSubmission {
            attempt_id: AttemptId::from_uuid(accepted),
            generation: 1,
            content: serde_json::json!({}),
            content_hash: support::digest('a'),
        },
    )
    .await
    .unwrap();
    let active = grant(&mut conn, tasks[1], epoch, limits)
        .await
        .unwrap()
        .attempt_id
        .unwrap();
    cancellation::request_cancellation(
        &mut conn,
        &cancellation::CancelJob {
            job_id: job,
            requested_by: None,
            reason: None,
        },
    )
    .await
    .unwrap();
    stop_current(
        &mut conn,
        job,
        epoch,
        JobState::Cancelled,
        "CancellationRequested",
    )
    .await
    .unwrap();
    assert_eq!(
        attempts::attempt(&mut conn, AttemptId::from_uuid(active))
            .await
            .unwrap()
            .state,
        bergsmith_common::task::AttemptState::Cancelled
    );
    assert_eq!(
        plans::task(&mut conn, tasks[2]).await.unwrap().state,
        bergsmith_common::task::TaskState::Cancelled
    );
    assert_eq!(
        plans::task(&mut conn, tasks[0]).await.unwrap().state,
        bergsmith_common::task::TaskState::Accepted
    );
    assert_eq!(
        attempts::attempt(&mut conn, AttemptId::from_uuid(accepted))
            .await
            .unwrap()
            .state,
        bergsmith_common::task::AttemptState::Accepted
    );
    stop_current(&mut conn, job, epoch, JobState::Cancelled, "repeat")
        .await
        .unwrap();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn expired_sessions_are_recovered_even_before_the_attempt_deadline(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (job, tasks) = configured(&mut conn, &tenant, 1).await;
    let epoch = support::leader(&mut conn).await;
    scheduler::admit(&mut conn, job, epoch, scheduler::AdmissionLimits::default())
        .await
        .unwrap();
    let id = grant(
        &mut conn,
        tasks[0],
        epoch,
        scheduler::AdmissionLimits::default(),
    )
    .await
    .unwrap()
    .attempt_id
    .unwrap();
    let session: Uuid = sqlx::query_scalar("SELECT worker_session_id FROM attempts WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    sqlx::query("UPDATE worker_sessions SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE id=$1").bind(session).execute(&mut *conn).await.unwrap();
    scheduler::reconcile_execution(&mut conn, job, epoch)
        .await
        .unwrap();
    assert_eq!(
        plans::task(&mut conn, tasks[0]).await.unwrap().state,
        bergsmith_common::task::TaskState::RetryWaiting
    );
    assert!(
        workers::session(
            &mut conn,
            &workers::Identity {
                organization: tenant.organization,
                principal: tenant.user,
                pool: "test".into()
            },
            WorkerSessionId::from_uuid(session)
        )
        .await
        .is_err()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn execution_ownership_and_publication_creation_cannot_bypass_each_other(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (owner, _) = configured(&mut conn, &tenant, 1).await;
    let (publication, _) = configured(&mut conn, &tenant, 1).await;
    let epoch = support::leader(&mut conn).await;
    assert!(
        scheduler::admit(
            &mut conn,
            owner,
            epoch,
            scheduler::AdmissionLimits::default()
        )
        .await
        .unwrap()
    );
    support::force_job_state(&mut conn, publication, JobState::ReadyToCommit).await;
    let error = bergsmith_persistence::publications::create_intent(
        &mut conn,
        &bergsmith_persistence::publications::Intent {
            job_id: publication,
            publication_id: bergsmith_common::ids::PublicationId::generate(),
            content: serde_json::json!({}),
            request_body: vec![],
            request_hash: support::digest('a'),
        },
        0,
        epoch,
        &[],
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        bergsmith_persistence::PersistenceError::AdmissionUnavailable
    ));
    stop_current(&mut conn, owner, epoch, JobState::Failed, "Stopped")
        .await
        .unwrap();
    let (next, _) = configured(&mut conn, &tenant, 1).await;
    assert!(
        !scheduler::admit(
            &mut conn,
            next,
            epoch,
            scheduler::AdmissionLimits::default()
        )
        .await
        .unwrap()
    );
    stop_current(&mut conn, publication, epoch, JobState::Failed, "Stopped")
        .await
        .unwrap();
    assert!(
        scheduler::admit(
            &mut conn,
            next,
            epoch,
            scheduler::AdmissionLimits::default()
        )
        .await
        .unwrap()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn global_tenant_and_pool_job_budgets_each_apply_independently(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let epoch = support::leader(&mut conn).await;
    for (index, limits) in [
        scheduler::AdmissionLimits {
            jobs: 1,
            organization_jobs: 100,
            pool_jobs: 100,
            ..scheduler::AdmissionLimits::default()
        },
        scheduler::AdmissionLimits {
            jobs: 100,
            organization_jobs: 1,
            pool_jobs: 100,
            ..scheduler::AdmissionLimits::default()
        },
        scheduler::AdmissionLimits {
            jobs: 100,
            organization_jobs: 100,
            pool_jobs: 1,
            ..scheduler::AdmissionLimits::default()
        },
    ]
    .into_iter()
    .enumerate()
    {
        let mut tenant = support::tenant(&mut conn, &format!("acme-{index}")).await;
        let (first, _) = configured(&mut conn, &tenant, 1).await;
        tenant.table = bergsmith_persistence::tables::resolve_table(
            &mut conn,
            tenant.organization,
            &support::alias(tenant.connection, "other"),
            Uuid::now_v7(),
        )
        .await
        .unwrap()
        .id;
        let (second, _) = configured(&mut conn, &tenant, 1).await;
        assert!(
            scheduler::admit(&mut conn, first, epoch, limits)
                .await
                .unwrap()
        );
        assert!(
            !scheduler::admit(&mut conn, second, epoch, limits)
                .await
                .unwrap()
        );
        stop_current(&mut conn, first, epoch, JobState::Failed, "Stopped")
            .await
            .unwrap();
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn memory_cpu_and_attempt_budgets_each_apply_independently(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let epoch = support::leader(&mut conn).await;
    for limits in [
        scheduler::AdmissionLimits {
            memory_bytes: 100,
            cpu_millis: 1_000,
            attempts: 2,
            ..scheduler::AdmissionLimits::default()
        },
        scheduler::AdmissionLimits {
            memory_bytes: 1_000,
            cpu_millis: 100,
            attempts: 2,
            ..scheduler::AdmissionLimits::default()
        },
        scheduler::AdmissionLimits {
            memory_bytes: 1_000,
            cpu_millis: 1_000,
            attempts: 1,
            ..scheduler::AdmissionLimits::default()
        },
    ] {
        let (job, tasks) = configured(&mut conn, &tenant, 2).await;
        assert!(
            scheduler::admit(&mut conn, job, epoch, limits)
                .await
                .unwrap()
        );
        grant(&mut conn, tasks[0], epoch, limits).await.unwrap();
        assert!(matches!(
            grant(&mut conn, tasks[1], epoch, limits).await.unwrap_err(),
            bergsmith_persistence::PersistenceError::AdmissionUnavailable
        ));
        stop_current(&mut conn, job, epoch, JobState::Failed, "Stopped")
            .await
            .unwrap();
    }
}

/// Stops a job at whatever revision it currently has.
async fn stop_current(
    conn: &mut PgConnection,
    job: JobId,
    epoch: i64,
    state: JobState,
    reason: &str,
) -> Result<bool, bergsmith_persistence::PersistenceError> {
    let revision: i64 = sqlx::query_scalar("SELECT state_revision FROM jobs WHERE id=$1")
        .bind(job.as_uuid())
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    scheduler::stop(conn, job, revision, epoch, state, reason).await
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_stop_decided_on_an_older_revision_leaves_the_moved_job_unchanged(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (job, _) = configured(&mut conn, &tenant, 1).await;
    let epoch = support::leader(&mut conn).await;
    let validated = jobs::job(&mut conn, tenant.organization, job)
        .await
        .unwrap();
    // Cancellation wins while external validation runs.
    cancellation::request_cancellation(
        &mut conn,
        &cancellation::CancelJob {
            job_id: job,
            requested_by: None,
            reason: None,
        },
    )
    .await
    .unwrap();
    assert!(
        !scheduler::stop(
            &mut conn,
            job,
            validated.state_revision,
            epoch,
            JobState::ReplanRequired,
            "PlanningBaselineMismatch",
        )
        .await
        .unwrap()
    );
    let current = jobs::job(&mut conn, tenant.organization, job)
        .await
        .unwrap();
    assert_eq!(current.state, JobState::Cancelling);
    assert!(
        scheduler::stop(
            &mut conn,
            job,
            current.state_revision,
            epoch,
            JobState::Cancelled,
            "CancellationRequested",
        )
        .await
        .unwrap()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn retry_timers_cover_every_job_whose_retries_reconciliation_releases(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (job, tasks) = configured(&mut conn, &tenant, 1).await;
    let epoch = support::leader(&mut conn).await;
    sqlx::query("UPDATE tasks SET state='RetryWaiting',retry_not_before=clock_timestamp()-interval '1 second' WHERE id=$1")
        .bind(tasks[0].as_uuid()).execute(&mut *conn).await.unwrap();
    // The job is still Queued: reconciliation would release this retry, so
    // the timer listing must report it.
    let timers = scheduler::retry_timers(&mut conn).await.unwrap();
    assert_eq!(timers.len(), 1);
    assert_eq!(timers[0].0, *tasks[0].as_uuid());
    scheduler::reconcile_execution(&mut conn, job, epoch)
        .await
        .unwrap();
    assert!(scheduler::retry_timers(&mut conn).await.unwrap().is_empty());
}

async fn need(
    conn: &mut PgConnection,
    organization: OrganizationId,
    job: JobId,
    limits: scheduler::AdmissionLimits,
) -> scheduler::ValidationNeed {
    let current = jobs::job(conn, organization, job).await.unwrap();
    scheduler::validation_need(conn, &current, limits)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn validation_is_needed_only_to_admit_or_release_a_due_retry(pool: PgPool) {
    use scheduler::ValidationNeed;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let org = tenant.organization;
    let (job, tasks) = configured(&mut conn, &tenant, 1).await;
    let epoch = support::leader(&mut conn).await;
    let limits = scheduler::AdmissionLimits::default();
    let one = scheduler::AdmissionLimits { jobs: 1, ..limits };
    assert_eq!(
        need(&mut conn, org, job, one).await,
        ValidationNeed::Admission
    );
    assert!(scheduler::admit(&mut conn, job, epoch, one).await.unwrap());
    // Another job waits for a full global budget without being validated.
    let (blocked, _) = configured(&mut conn, &tenant, 1).await;
    assert_eq!(
        need(&mut conn, org, blocked, one).await,
        ValidationNeed::None
    );
    assert_eq!(need(&mut conn, org, job, one).await, ValidationNeed::None);
    let poll = grant(&mut conn, tasks[0], epoch, limits).await.unwrap();
    sqlx::query("UPDATE attempts SET granted_at=clock_timestamp()-interval '2 minutes',lease_expires_at=clock_timestamp()-interval '1 minute' WHERE id=$1")
        .bind(poll.attempt_id.unwrap()).execute(&mut *conn).await.unwrap();
    // Repair fences the expired attempt and schedules a future retry without
    // releasing it.
    scheduler::repair_attempts(&mut conn, job, epoch)
        .await
        .unwrap();
    assert_eq!(
        plans::task(&mut conn, tasks[0]).await.unwrap().state,
        bergsmith_common::task::TaskState::RetryWaiting
    );
    assert_eq!(need(&mut conn, org, job, one).await, ValidationNeed::None);
    sqlx::query(
        "UPDATE tasks SET retry_not_before=clock_timestamp()-interval '1 second' WHERE id=$1",
    )
    .bind(tasks[0].as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    scheduler::repair_attempts(&mut conn, job, epoch)
        .await
        .unwrap();
    assert_eq!(
        plans::task(&mut conn, tasks[0]).await.unwrap().state,
        bergsmith_common::task::TaskState::RetryWaiting
    );
    assert_eq!(
        need(&mut conn, org, job, one).await,
        ValidationNeed::RetryRelease
    );
}
