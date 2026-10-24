//! Worker authority and clock checks under real PostgreSQL lock contention.
#![expect(clippy::unwrap_used, reason = "fixtures fail on setup errors")]

mod support;

use bergsmith_common::disposition::FailureDisposition;
use bergsmith_common::ids::{AttemptId, PrincipalId, WorkerSessionId};
use bergsmith_common::job::JobState;
use bergsmith_persistence::{PersistenceError, attempts, workers};
use sqlx::{Connection, PgConnection, PgPool};
use std::time::Duration;
use uuid::Uuid;

async fn identity(conn: &mut PgConnection, session: WorkerSessionId) -> workers::Identity {
    let (organization, principal, pool): (Uuid, Uuid, String) = sqlx::query_as(
        "SELECT organization_id, principal_id, pool FROM worker_sessions WHERE id = $1",
    )
    .bind(session.as_uuid())
    .fetch_one(conn)
    .await
    .unwrap();
    workers::Identity {
        organization: bergsmith_common::ids::OrganizationId::from_uuid(organization),
        principal: PrincipalId::from_uuid(principal),
        pool,
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn registration_requires_a_service_principal_and_sorted_unique_capabilities(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let identity = workers::Identity {
        organization: tenant.organization,
        principal: tenant.user,
        pool: "native".into(),
    };
    let mut registration = workers::Registration {
        id: WorkerSessionId::generate(),
        worker_name: "a".into(),
        capabilities: vec!["native".into()],
        memory_bytes: 1024,
        cpu_millis: 1000,
        build_version: "test".into(),
    };
    assert!(
        workers::register(&mut conn, &identity, &registration, Duration::from_secs(90))
            .await
            .is_err()
    );
    registration.capabilities.push("native".into());
    assert!(
        workers::register(&mut conn, &identity, &registration, Duration::from_secs(90))
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM worker_sessions")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_task_resource_envelope_survives_reconnection_and_cannot_be_changed(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    let task = support::plan(&mut conn, job, 1).await[0];
    workers::bind_envelope(&mut conn, task, b"original")
        .await
        .unwrap();
    drop(conn);
    let mut conn = pool.acquire().await.unwrap();
    workers::bind_envelope(&mut conn, task, b"original")
        .await
        .unwrap();
    assert!(
        workers::bind_envelope(&mut conn, task, b"changed")
            .await
            .is_err()
    );
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT content FROM task_resource_envelopes WHERE task_id = $1")
            .bind(task.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(stored, b"original");
    assert!(
        sqlx::query("UPDATE task_resource_envelopes SET content = 'changed' WHERE task_id = $1")
            .bind(task.as_uuid())
            .execute(&mut *conn)
            .await
            .is_err()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn grant_requires_current_leadership_and_does_not_consume_an_attempt_on_rejection(
    pool: PgPool,
) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    let task = support::plan(&mut conn, job, 1).await[0];
    support::force_job_state(&mut conn, job, JobState::Queued).await;
    let session = support::worker_session(&mut conn, task).await;
    let identity = identity(&mut conn, session).await;
    let error = workers::grant(
        &mut conn,
        &identity,
        &attempts::NewGrant {
            attempt_id: AttemptId::generate(),
            task_id: task,
            worker_session_id: session,
            lease: Duration::from_secs(90),
        },
        Uuid::now_v7(),
        42,
        &workers::GrantPayload {
            grant_bytes: b"safe",
            output_prefix: "file:///tmp/output/",
            envelope_bytes: b"original",
            generation: 1,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::LeadershipLost { epoch: 42 }
    ));
    let attempts: i32 = sqlx::query_scalar("SELECT attempts_started FROM tasks WHERE id = $1")
        .bind(task.as_uuid())
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(attempts, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_grant_encoded_for_an_outdated_generation_persists_nothing(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    let task = support::plan(&mut conn, job, 1).await[0];
    support::force_job_state(&mut conn, job, JobState::Queued).await;
    let session = support::worker_session(&mut conn, task).await;
    let identity = identity(&mut conn, session).await;
    let epoch = support::leader(&mut conn).await;
    // The caller read the task before another attempt started and ended.
    sqlx::query("UPDATE tasks SET attempts_started = 1 WHERE id = $1")
        .bind(task.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let new = attempts::NewGrant {
        attempt_id: AttemptId::generate(),
        task_id: task,
        worker_session_id: session,
        lease: Duration::from_secs(90),
    };
    let request = Uuid::now_v7();
    let payload = |generation| workers::GrantPayload {
        grant_bytes: b"safe",
        output_prefix: "file:///tmp/output/",
        envelope_bytes: b"original",
        generation,
    };
    let error = workers::grant(&mut conn, &identity, &new, request, epoch, &payload(1))
        .await
        .unwrap_err();
    assert!(
        matches!(error, PersistenceError::StaleAttempt { attempt } if attempt == new.attempt_id)
    );
    let (attempts, polls, started): (i64, i64, i32) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM attempts), (SELECT count(*) FROM worker_polls),
            (SELECT attempts_started FROM tasks WHERE id = $1)",
    )
    .bind(task.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!((attempts, polls, started), (0, 0, 1));
    let redrafted = workers::grant(&mut conn, &identity, &new, request, epoch, &payload(2))
        .await
        .unwrap();
    assert_eq!(redrafted.attempt_id, Some(*new.attempt_id.as_uuid()));
    let attempt = attempts::attempt(&mut conn, new.attempt_id).await.unwrap();
    assert_eq!(attempt.generation, 2);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn poll_and_grant_are_immutable_and_foreign_session_is_rejected(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    let task = support::plan(&mut conn, job, 1).await[0];
    support::force_job_state(&mut conn, job, JobState::Queued).await;
    let session = support::worker_session(&mut conn, task).await;
    let identity = identity(&mut conn, session).await;
    let epoch = support::leader(&mut conn).await;
    let request = Uuid::now_v7();
    let new = attempts::NewGrant {
        attempt_id: AttemptId::generate(),
        task_id: task,
        worker_session_id: session,
        lease: Duration::from_secs(90),
    };
    let first = workers::grant(
        &mut conn,
        &identity,
        &new,
        request,
        epoch,
        &workers::GrantPayload {
            grant_bytes: b"safe",
            output_prefix: "file:///tmp/output/",
            envelope_bytes: b"original",
            generation: 1,
        },
    )
    .await
    .unwrap();
    let replay = workers::grant(
        &mut conn,
        &identity,
        &new,
        request,
        epoch,
        &workers::GrantPayload {
            grant_bytes: b"different",
            output_prefix: "file:///tmp/other/",
            envelope_bytes: b"original",
            generation: 1,
        },
    )
    .await
    .unwrap();
    assert_eq!(first.grant_bytes, replay.grant_bytes);
    assert!(
        sqlx::query("UPDATE worker_polls SET grant_bytes = 'changed'")
            .execute(&mut *conn)
            .await
            .is_err()
    );
    let mut foreign = identity.clone();
    foreign.principal = PrincipalId::generate();
    assert!(
        workers::owned_attempt(&mut conn, &foreign, session, new.attempt_id)
            .await
            .is_err()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_renewal_never_outlasts_the_jobs_execution_budget(pool: PgPool) {
    use chrono::{DateTime, Utc};
    let (job, attempt) = support::running_attempt(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    let identity = identity(&mut conn, attempt.worker_session_id).await;
    let epoch = support::leader(&mut conn).await;

    // The grant gave the attempt a lease of its own; the job's budget is
    // shorter, and a heartbeat must not extend it.
    sqlx::query(
        "UPDATE jobs SET deadline_at = clock_timestamp() + interval '5 seconds' WHERE id = $1",
    )
    .bind(job.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    let deadline: DateTime<Utc> = sqlx::query_scalar("SELECT deadline_at FROM jobs WHERE id = $1")
        .bind(job.as_uuid())
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let renewal = workers::renew(
        &mut conn,
        &identity,
        attempt.worker_session_id,
        attempt.id,
        attempt.generation,
        epoch,
        Duration::from_secs(90),
    )
    .await
    .unwrap();
    let workers::Renewal::Renewed(lease) = renewal else {
        panic!("the heartbeat renews an attempt inside its budget");
    };
    assert!(
        lease <= deadline,
        "the renewal must not outlast the job's budget"
    );

    // Once the budget is spent, the heartbeat fences the attempt instead of
    // renewing it.
    sqlx::query(
        "UPDATE jobs SET deadline_at = clock_timestamp() - interval '1 second' WHERE id = $1",
    )
    .bind(job.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        workers::renew(
            &mut conn,
            &identity,
            attempt.worker_session_id,
            attempt.id,
            attempt.generation,
            epoch,
            Duration::from_secs(90),
        )
        .await
        .unwrap(),
        workers::Renewal::Fenced
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn heartbeat_waiting_for_the_job_lock_cannot_revive_an_expired_attempt(pool: PgPool) {
    renewal_wait(&pool, false).await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn heartbeat_waiting_for_the_session_lock_cannot_revive_an_expired_session(pool: PgPool) {
    renewal_wait(&pool, true).await;
}

async fn renewal_wait(pool: &PgPool, expire_session: bool) {
    let (job, attempt) = support::running_attempt(pool, "acme").await;
    let mut blocker = pool.acquire().await.unwrap();
    let identity = identity(&mut blocker, attempt.worker_session_id).await;
    let epoch = support::leader(&mut blocker).await;
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let mut waiter = pool.acquire().await.unwrap();
    let waiter_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *waiter)
        .await
        .unwrap();
    let mut tx = blocker.begin().await.unwrap();
    if expire_session {
        sqlx::query("SELECT id FROM worker_sessions WHERE id = $1 FOR UPDATE")
            .bind(attempt.worker_session_id.as_uuid())
            .execute(&mut *tx)
            .await
            .unwrap();
    } else {
        sqlx::query("SELECT id FROM jobs WHERE id = $1 FOR UPDATE")
            .bind(job.as_uuid())
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    let (renewal, ()) = tokio::join!(
        workers::renew(
            &mut waiter,
            &identity,
            attempt.worker_session_id,
            attempt.id,
            attempt.generation,
            epoch,
            Duration::from_secs(90)
        ),
        async {
            support::wait_until_blocked(&mut tx, waiter_pid, blocker_pid).await;
            if expire_session {
                sqlx::query(
                    "UPDATE worker_sessions SET lease_expires_at = s.xact_start +
                (clock_timestamp() - s.xact_start) / 2 FROM pg_stat_activity s
                WHERE worker_sessions.id = $1 AND s.pid = $2",
                )
                .bind(attempt.worker_session_id.as_uuid())
                .bind(waiter_pid)
                .execute(&mut *tx)
                .await
                .unwrap();
            } else {
                sqlx::query(
                    "UPDATE attempts SET lease_expires_at = s.xact_start +
                (clock_timestamp() - s.xact_start) / 2 FROM pg_stat_activity s
                WHERE attempts.id = $1 AND s.pid = $2",
                )
                .bind(attempt.id.as_uuid())
                .bind(waiter_pid)
                .execute(&mut *tx)
                .await
                .unwrap();
            }
            tx.commit().await.unwrap();
        }
    );
    if expire_session {
        assert!(renewal.is_err());
    } else {
        assert_eq!(renewal.unwrap(), workers::Renewal::Fenced);
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn failure_classification_is_authoritative_and_replay_does_not_consume_attempts(
    pool: PgPool,
) {
    let (_, attempt) = support::running_attempt(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    let identity = identity(&mut conn, attempt.worker_session_id).await;
    let epoch = support::leader(&mut conn).await;
    let first = workers::report_failure(
        &mut conn,
        &identity,
        attempt.worker_session_id,
        attempt.id,
        attempt.generation,
        epoch,
        "StorageOperationTimedOut",
    )
    .await
    .unwrap();
    assert_eq!(first, (FailureDisposition::RetryTask, JobState::Running));
    assert_eq!(
        first,
        workers::report_failure(
            &mut conn,
            &identity,
            attempt.worker_session_id,
            attempt.id,
            attempt.generation,
            epoch,
            "StorageOperationTimedOut"
        )
        .await
        .unwrap()
    );
    assert!(
        workers::report_failure(
            &mut conn,
            &identity,
            attempt.worker_session_id,
            attempt.id,
            attempt.generation,
            epoch,
            "DifferentFailure"
        )
        .await
        .is_err()
    );
    let task = bergsmith_persistence::plans::task(&mut conn, attempt.task_id)
        .await
        .unwrap();
    assert_eq!(task.state, bergsmith_common::task::TaskState::RetryWaiting);
    assert_eq!(task.attempts_started, 1);
    assert!(task.retry_not_before.is_some());
}
