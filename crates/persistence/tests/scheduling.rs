//! Atomic admission, immutable dispatch, and database-time retry recovery.
#![expect(clippy::unwrap_used, reason = "integration setup and assertions")]

mod support;

use bergsmith_common::ids::{
    AttemptId, JobId, OrganizationId, PrincipalId, PublicationId, TaskId, WorkerSessionId,
};
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_persistence::{
    attempts, cancellation, jobs, plans, publications, scheduling as scheduler, workers,
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

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn admission_waiting_for_a_connection_edit_observes_disable(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let epoch = support::leader(&mut conn).await;
    let (job, _) = configured(&mut conn, &tenant, 1).await;
    let mut editing = pool.begin().await.unwrap();
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *editing)
        .await
        .unwrap();
    sqlx::query("UPDATE catalog_connections SET enabled=false WHERE id=$1")
        .bind(tenant.connection.as_uuid())
        .execute(&mut *editing)
        .await
        .unwrap();
    let mut admitting = pool.acquire().await.unwrap();
    let waiter: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *admitting)
        .await
        .unwrap();
    let admission = tokio::spawn(async move {
        scheduler::admit(
            &mut admitting,
            job,
            epoch,
            scheduler::AdmissionLimits::default(),
        )
        .await
    });
    support::wait_until_blocked(&mut conn, waiter, blocker).await;
    editing.commit().await.unwrap();
    assert!(!admission.await.unwrap().unwrap());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn disabled_connections_block_new_admission_but_preserve_admitted_jobs(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let epoch = support::leader(&mut conn).await;
    let (job, _) = configured(&mut conn, &tenant, 1).await;
    sqlx::query("UPDATE catalog_connections SET enabled=false WHERE id=$1")
        .bind(tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(
        !scheduler::admit(&mut conn, job, epoch, scheduler::AdmissionLimits::default())
            .await
            .unwrap()
    );
    sqlx::query("UPDATE catalog_connections SET enabled=true WHERE id=$1")
        .bind(tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(
        scheduler::admit(&mut conn, job, epoch, scheduler::AdmissionLimits::default())
            .await
            .unwrap()
    );
    sqlx::query("UPDATE catalog_connections SET enabled=false WHERE id=$1")
        .bind(tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(
        scheduler::admit(&mut conn, job, epoch, scheduler::AdmissionLimits::default())
            .await
            .unwrap()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_paused_connection_blocks_new_admission_but_preserves_admitted_jobs(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let epoch = support::leader(&mut conn).await;
    let (job, _) = configured(&mut conn, &tenant, 1).await;
    sqlx::query("UPDATE catalog_connections SET maintenance_enabled=false WHERE id=$1")
        .bind(tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(
        !scheduler::admit(&mut conn, job, epoch, scheduler::AdmissionLimits::default())
            .await
            .unwrap()
    );
    sqlx::query("UPDATE catalog_connections SET maintenance_enabled=true WHERE id=$1")
        .bind(tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(
        scheduler::admit(&mut conn, job, epoch, scheduler::AdmissionLimits::default())
            .await
            .unwrap()
    );
    // Pausing again does not disturb the admitted job: its admission and its
    // state stay, and the pause only stops new work.
    sqlx::query("UPDATE catalog_connections SET maintenance_enabled=false WHERE id=$1")
        .bind(tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let admitted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM execution_admissions WHERE job_id = $1 AND NOT released",
    )
    .bind(job.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(admitted, 1, "the admitted job keeps its admission");
    let state = jobs::job_by_id(&mut conn, job).await.unwrap().state;
    assert_eq!(
        state,
        JobState::Queued,
        "the pause does not change the state of an admitted job"
    );
    // A job submitted after the pause waits instead of running.
    let next = support::job(&mut conn, &tenant).await;
    let next_tasks = support::plan(&mut conn, next, 1).await;
    support::force_job_state(&mut conn, next, JobState::Queued).await;
    scheduler::register_dispatch(
        &mut conn,
        next,
        &next_tasks.iter().copied().map(spec).collect::<Vec<_>>(),
        scheduler::RetryPolicy::default(),
    )
    .await
    .unwrap();
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
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_job_without_a_deadline_is_not_bounded_by_a_budget(pool: PgPool) {
    use chrono::Duration as ChronoDuration;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (job, tasks) = configured(&mut conn, &tenant, 1).await;
    // A job created before the budget existed carries no deadline.
    sqlx::query("UPDATE jobs SET deadline_at = NULL WHERE id = $1")
        .bind(job.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();

    assert_eq!(
        jobs::remaining_execution_budget(&mut conn, job)
            .await
            .unwrap(),
        None
    );
    let attempt = support::grant(&mut conn, tasks[0]).await.unwrap();
    let granted_at = attempt.granted_at;
    // The worker's own lease applies, not a capped one.
    assert!(attempt.lease_expires_at > granted_at + ChronoDuration::seconds(60));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_one_second_budget_bounds_the_plan_and_its_attempt(pool: PgPool) {
    use chrono::{Duration as ChronoDuration, Utc};
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    let tasks = support::plan_with_budget(&mut conn, job, 1, Duration::from_secs(1)).await;
    support::force_job_state(&mut conn, job, JobState::Queued).await;
    let deadline: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT deadline_at FROM jobs WHERE id = $1")
            .bind(job.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    let created: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT created_at FROM plans WHERE job_id = $1")
            .bind(job.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    // The budget starts with the plan, so the deadline is about a second after
    // it; the margin only absorbs the database clock's sub-millisecond steps.
    assert!(deadline >= created + ChronoDuration::milliseconds(500));
    assert!(deadline <= created + ChronoDuration::seconds(2));
    let remaining = jobs::remaining_execution_budget(&mut conn, job)
        .await
        .unwrap()
        .expect("a planned job carries its budget");
    assert!(remaining > Duration::ZERO);
    assert!(remaining <= Duration::from_secs(1));

    // The attempt fits inside what is left, and it is still a future lease.
    let attempt = support::grant(&mut conn, tasks[0]).await.unwrap();
    assert!(attempt.lease_expires_at > attempt.granted_at);
    assert!(attempt.lease_expires_at <= deadline);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_attempt_never_outlasts_the_jobs_execution_budget(pool: PgPool) {
    use chrono::{DateTime, Duration as ChronoDuration, Utc};
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (job, tasks) = configured(&mut conn, &tenant, 3).await;

    // The plan gave the job a budget, so it has a deadline.
    let deadline: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT deadline_at FROM jobs WHERE id = $1")
            .bind(job.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert!(deadline.is_some(), "a planned job carries its budget");

    // A short budget caps the lease: the worker's own lease is far longer.
    sqlx::query(
        "UPDATE jobs SET deadline_at = clock_timestamp() + interval '5 seconds' WHERE id = $1",
    )
    .bind(job.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let attempt = support::grant(&mut conn, tasks[0]).await.unwrap();
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert!(
        attempt.lease_expires_at <= now + ChronoDuration::seconds(5),
        "the lease must not outlast the job's budget"
    );

    // The second task of the same job may only use what is left of it.
    let second = support::grant(&mut conn, tasks[1]).await.unwrap();
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert!(
        second.lease_expires_at <= now + ChronoDuration::seconds(5),
        "a sequential task must fit in the remaining budget"
    );

    // Once the budget is spent, no further attempt is granted.
    sqlx::query(
        "UPDATE jobs SET deadline_at = clock_timestamp() - interval '1 second' WHERE id = $1",
    )
    .bind(job.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let error = support::grant(&mut conn, tasks[2]).await.unwrap_err();
    assert!(matches!(
        error,
        bergsmith_persistence::PersistenceError::ExecutionBudgetExceeded { .. }
    ));
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM attempts WHERE task_id = $1")
        .bind(tasks[2].as_uuid())
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(
        attempts, 0,
        "no attempt is created after the budget is spent"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_paused_connection_cannot_borrow_another_connections_grant(pool: PgPool) {
    use bergsmith_common::permission::{GrantScope, Role};
    use bergsmith_persistence::grants::{self, NewRoleGrant};
    use bergsmith_persistence::tables;
    let mut conn = pool.acquire().await.unwrap();
    let organization = support::organization(&mut conn, "acme").await;
    let paused = support::connection(&mut conn, organization, "paused").await;
    let usable = support::connection(&mut conn, organization, "usable").await;
    let table_uuid = Uuid::now_v7();
    let table = tables::resolve_table(
        &mut conn,
        organization,
        &support::alias(paused, "events"),
        table_uuid,
    )
    .await
    .unwrap()
    .id;
    // The same table is served by the second connection.
    tables::resolve_table(
        &mut conn,
        organization,
        &support::alias(usable, "events"),
        table_uuid,
    )
    .await
    .unwrap();
    // The requester may submit through the paused connection only.
    let requester = support::user(&mut conn, organization, "operator").await;
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: bergsmith_common::ids::RoleGrantId::generate(),
            organization_id: organization,
            principal_id: requester,
            role: Role::Operator,
            scope: GrantScope::connection(paused),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE catalog_connections SET maintenance_enabled=false WHERE id=$1")
        .bind(paused.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let epoch = support::leader(&mut conn).await;
    let job = jobs::create_job(
        &mut conn,
        &jobs::NewJob {
            id: JobId::generate(),
            organization_id: organization,
            table_id: table,
            operation: JobOperation::Compact,
            retry_of: None,
            created_by: requester,
        },
    )
    .await
    .unwrap()
    .id;
    let tasks = support::plan(&mut conn, job, 1).await;
    support::force_job_state(&mut conn, job, JobState::Queued).await;
    scheduler::register_dispatch(
        &mut conn,
        job,
        &tasks.iter().copied().map(spec).collect::<Vec<_>>(),
        scheduler::RetryPolicy::default(),
    )
    .await
    .unwrap();

    // The paused connection is not a usable route, and the requester's grant
    // does not cover the connection that is.
    assert!(
        !scheduler::admit(&mut conn, job, epoch, scheduler::AdmissionLimits::default())
            .await
            .unwrap()
    );

    // With the maintenance pause lifted, the requester's own grant covers the
    // connection again, so the pause was the blocker.
    sqlx::query("UPDATE catalog_connections SET maintenance_enabled=true WHERE id=$1")
        .bind(paused.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(
        scheduler::admit(&mut conn, job, epoch, scheduler::AdmissionLimits::default())
            .await
            .unwrap()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_inaccessible_enabled_alias_cannot_bypass_a_disabled_connection(pool: PgPool) {
    use bergsmith_common::permission::{GrantScope, Role};
    use bergsmith_persistence::tables;
    let mut conn = pool.acquire().await.unwrap();
    let mut tenant = support::tenant(&mut conn, "acme").await;
    let hidden = support::connection(&mut conn, tenant.organization, "hidden").await;
    let identity = tables::table_identity(&mut conn, tenant.organization, tenant.table)
        .await
        .unwrap();
    tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(hidden, "events"),
        identity.table_uuid,
    )
    .await
    .unwrap();
    let operator = support::user(&mut conn, tenant.organization, "operator").await;
    bergsmith_persistence::grants::create_grant(
        &mut conn,
        &bergsmith_persistence::grants::NewRoleGrant {
            id: bergsmith_common::ids::RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: operator,
            role: Role::Operator,
            scope: GrantScope::connection(tenant.connection),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE catalog_connections SET enabled=false WHERE id=$1")
        .bind(tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    tenant.user = operator;
    let epoch = support::leader(&mut conn).await;
    let (job, _) = configured(&mut conn, &tenant, 1).await;
    assert!(
        !scheduler::admit(&mut conn, job, epoch, scheduler::AdmissionLimits::default())
            .await
            .unwrap()
    );
    bergsmith_persistence::grants::create_grant(
        &mut conn,
        &bergsmith_persistence::grants::NewRoleGrant {
            id: bergsmith_common::ids::RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: operator,
            role: Role::Operator,
            scope: GrantScope::table(tenant.table),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    assert!(
        scheduler::admit(&mut conn, job, epoch, scheduler::AdmissionLimits::default())
            .await
            .unwrap()
    );
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
    let generation: i32 = sqlx::query_scalar("SELECT attempts_started + 1 FROM tasks WHERE id=$1")
        .bind(task.as_uuid())
        .fetch_one(&mut *conn)
        .await
        .unwrap();
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
            generation,
        },
        limits,
    )
    .await
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn dispatch_configuration_is_complete_atomic_and_immutable(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (job, tasks) = configured(&mut conn, &tenant, 3).await;
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
    let (job, tasks) = configured(&mut conn, &tenant, 3).await;
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
    let error = publications::create_intent(
        &mut conn,
        &publications::Intent {
            job_id: publication,
            publication_id: PublicationId::generate(),
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
        let (job, tasks) = configured(&mut conn, &tenant, 3).await;
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

#[sqlx::test]
async fn unauthorized_publication_intents_are_listed_for_resume(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "resume").await;
    let job = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, job, JobState::ReadyToCommit).await;
    let publication = PublicationId::generate();
    sqlx::query("INSERT INTO publications (id, job_id) VALUES ($1, $2)")
        .bind(publication.as_uuid())
        .bind(job.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO publication_intents
             (job_id, publication_id, content, request_body, request_hash)
         VALUES ($1, $2, '{}'::jsonb, $3, $4)",
    )
    .bind(job.as_uuid())
    .bind(publication.as_uuid())
    .bind(Vec::<u8>::new())
    .bind("0".repeat(64))
    .execute(&mut *conn)
    .await
    .unwrap();
    support::force_job_state(&mut conn, job, JobState::Committing).await;

    let found = publications::unauthorized_sends_after(&mut conn, Uuid::nil(), 8)
        .await
        .unwrap();
    assert_eq!(found.iter().map(|job| job.id).collect::<Vec<_>>(), [job]);

    sqlx::query("UPDATE jobs SET publication_started_at = now() WHERE id = $1")
        .bind(job.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(
        publications::unauthorized_sends_after(&mut conn, Uuid::nil(), 8)
            .await
            .unwrap()
            .is_empty(),
        "an authorized send belongs to evidence recovery"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn planning_and_dispatch_writes_are_fenced_after_leadership_handoff(pool: PgPool) {
    use bergsmith_persistence::{PersistenceError, leadership};
    use sqlx::Connection as _;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    let first = leadership::acquire_leadership(&mut conn, "first", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    {
        let mut tx = conn.begin().await.unwrap();
        let locked = plans::lock_for_planning(&mut tx, job, 0, first.epoch)
            .await
            .unwrap();
        assert_eq!(locked.state, JobState::Planning);
        tx.rollback().await.unwrap();
    }
    leadership::release_leadership(&mut conn, &first.holder, first.epoch)
        .await
        .unwrap();
    let second = support::leader(&mut conn).await;
    {
        let mut tx = conn.begin().await.unwrap();
        assert!(matches!(
            plans::lock_for_planning(&mut tx, job, 0, first.epoch).await,
            Err(PersistenceError::LeadershipLost { .. })
        ));
        tx.rollback().await.unwrap();
    }
    let tasks = support::plan(&mut conn, job, 1).await;
    let queued = jobs::transition_job(
        &mut conn,
        &jobs::Transition {
            job_id: job,
            expected_revision: 0,
            to: JobState::Queued,
            reason: None,
            actor: None,
            non_publication_proven: false,
        },
    )
    .await
    .unwrap();
    let specs = vec![spec(tasks[0])];
    assert!(matches!(
        scheduler::register_dispatch_at_epoch(
            &mut conn,
            job,
            &specs,
            scheduler::RetryPolicy::default(),
            first.epoch,
        )
        .await,
        Err(PersistenceError::LeadershipLost { .. })
    ));
    assert!(
        scheduler::dispatch_registration(&mut conn, job)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !scheduler::stop(
            &mut conn,
            job,
            0,
            second,
            JobState::Failed,
            "StalePlanningError"
        )
        .await
        .unwrap()
    );
    assert_eq!(jobs::job_by_id(&mut conn, job).await.unwrap(), queued);
    scheduler::register_dispatch_at_epoch(
        &mut conn,
        job,
        &specs,
        scheduler::RetryPolicy::default(),
        second,
    )
    .await
    .unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert!(matches!(
        plans::lock_for_planning(&mut tx, job, 0, second).await,
        Err(PersistenceError::RevisionConflict { .. })
    ));
    tx.rollback().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert!(matches!(
        plans::lock_for_planning(&mut tx, job, queued.state_revision, second).await,
        Err(PersistenceError::JobNotInState { .. })
    ));
}
