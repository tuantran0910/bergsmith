//! Jobs, plans, tasks, attempts, leases, receipts, and publications against
//! real PostgreSQL (BS-003.T3).

mod support;

use bergsmith_common::ids::{JobId, PlanId, PublicationId};
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_common::task::{AttemptState, TaskState};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::attempts::{
    self, AcceptOutcome, Acceptance, EndAttempt, ReceiptSubmission,
};
use bergsmith_persistence::jobs::{self, JobEventKind, NewJob};
use bergsmith_persistence::leadership::acquire_leadership;
use bergsmith_persistence::plans::{self, NewPlan};
use bergsmith_persistence::publications;
use chrono::Utc;
use sqlx::PgPool;
use std::time::Duration;
use support::{
    constraint, digest, expire_lease, force_job_state, grant, job, plan, release_task,
    running_attempt, sqlstate, tenant,
};

fn receipt(attempt: &attempts::Attempt, digit: char) -> ReceiptSubmission {
    ReceiptSubmission {
        attempt_id: attempt.id,
        generation: attempt.generation,
        content: serde_json::json!({ "files": [digit.to_string()] }),
        content_hash: digest(digit),
    }
}

fn fail_for_retry(attempt: &attempts::Attempt) -> EndAttempt {
    EndAttempt {
        attempt_id: attempt.id,
        generation: attempt.generation,
        attempt_state: AttemptState::Failed,
        task_state: TaskState::RetryWaiting,
        retry_not_before: Some(Utc::now()),
    }
}

// Jobs

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn jobs_start_planning_at_revision_zero_with_a_created_event(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;

    let job = jobs::job(&mut conn, tenant.organization, id).await.unwrap();
    let events = jobs::job_events(&mut conn, id).await.unwrap();

    assert_eq!(job.state, JobState::Planning);
    assert_eq!(job.state_revision, 0);
    assert_eq!(job.table_id, tenant.table);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, JobEventKind::Created);
    assert_eq!(events[0].actor, Some(tenant.user));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn jobs_cannot_reference_another_organizations_table(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = tenant(&mut conn, "acme").await;
    let globex = tenant(&mut conn, "globex").await;

    let error = jobs::create_job(
        &mut conn,
        &NewJob {
            id: JobId::generate(),
            organization_id: globex.organization,
            table_id: acme.table,
            operation: JobOperation::Compact,
            retry_of: None,
            created_by: globex.user,
        },
    )
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        PersistenceError::MissingReference { entity: "job", .. }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn jobs_are_not_visible_to_other_organizations(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = tenant(&mut conn, "acme").await;
    let globex = tenant(&mut conn, "globex").await;
    let id = job(&mut conn, &acme).await;

    let error = jobs::job(&mut conn, globex.organization, id)
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        PersistenceError::NotFound { entity: "job", .. }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_retry_links_to_the_job_it_retries(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let original = job(&mut conn, &tenant).await;

    let retry = jobs::create_job(
        &mut conn,
        &NewJob {
            id: JobId::generate(),
            organization_id: tenant.organization,
            table_id: tenant.table,
            operation: JobOperation::Compact,
            retry_of: Some(original),
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();

    assert_eq!(retry.retry_of, Some(original));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn job_events_are_immutable(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    job(&mut conn, &tenant).await;

    for statement in [
        "UPDATE job_events SET reason = 'edited'",
        "DELETE FROM job_events",
    ] {
        let error = sqlx::query(statement).execute(&pool).await.unwrap_err();
        assert_eq!(sqlstate(&error), "23001", "{statement}");
    }
}

// Plans and tasks

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_plan_creates_pending_tasks_in_ordinal_order(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    let task_ids = plan(&mut conn, id, 3).await;

    let plan = plans::plan_for_job(&mut conn, id).await.unwrap().unwrap();
    let tasks = plans::tasks(&mut conn, plan.id).await.unwrap();

    assert_eq!(tasks.iter().map(|t| t.id).collect::<Vec<_>>(), task_ids);
    assert_eq!(
        tasks.iter().map(|t| t.ordinal).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert!(
        tasks
            .iter()
            .all(|t| t.state == TaskState::Pending && t.attempts_started == 0)
    );
    assert_eq!(plan.content, serde_json::json!({ "groups": 3 }));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_job_has_at_most_one_plan(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    plan(&mut conn, id, 1).await;

    let error = plans::create_plan(
        &mut conn,
        &NewPlan {
            id: PlanId::generate(),
            job_id: id,
            content: serde_json::json!({}),
            content_hash: digest('b'),
            execution_budget: Duration::from_secs(180),
            task_ids: Vec::new(),
        },
    )
    .await
    .unwrap_err();

    assert_eq!(constraint(&error), "plans_job_id_key");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn only_planning_jobs_accept_a_plan(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;

    for state in [
        JobState::Queued,
        JobState::Running,
        JobState::Failed,
        JobState::NoOp,
    ] {
        let id = job(&mut conn, &tenant).await;
        force_job_state(&mut conn, id, state).await;

        let error = plans::create_plan(
            &mut conn,
            &NewPlan {
                id: PlanId::generate(),
                job_id: id,
                content: serde_json::json!({}),
                content_hash: digest('a'),
                execution_budget: Duration::from_secs(180),
                task_ids: Vec::new(),
            },
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, PersistenceError::JobNotInState { state: s, .. } if s == state),
            "{state}"
        );
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn plan_hashes_must_be_sha256_hex(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;

    let error = plans::create_plan(
        &mut conn,
        &NewPlan {
            id: PlanId::generate(),
            job_id: id,
            content: serde_json::json!({}),
            content_hash: "ABC".to_owned(),
            execution_budget: Duration::from_secs(180),
            task_ids: Vec::new(),
        },
    )
    .await
    .unwrap_err();

    assert_eq!(constraint(&error), "plans_content_hash_check");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn plans_are_immutable(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    plan(&mut conn, id, 0).await;

    for statement in ["UPDATE plans SET content = '{}'", "DELETE FROM plans"] {
        let error = sqlx::query(statement).execute(&pool).await.unwrap_err();
        assert_eq!(sqlstate(&error), "23001", "{statement}");
    }
}

// Attempts and leases

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_grant_leases_the_task_with_the_next_generation(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    let task = plan(&mut conn, id, 1).await[0];
    force_job_state(&mut conn, id, JobState::Queued).await;

    let attempt = grant(&mut conn, task).await.unwrap();
    let leased = plans::task(&mut conn, task).await.unwrap();

    assert_eq!(attempt.generation, 1);
    assert_eq!(attempt.state, AttemptState::Granted);
    assert!(attempt.lease_expires_at > attempt.granted_at);
    assert_eq!(leased.state, TaskState::Leased);
    assert_eq!(leased.attempts_started, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn grants_need_a_queued_or_running_job(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;

    for state in [
        JobState::Planning,
        JobState::Cancelling,
        JobState::ReadyToCommit,
    ] {
        let id = job(&mut conn, &tenant).await;
        let task = plan(&mut conn, id, 1).await[0];
        force_job_state(&mut conn, id, state).await;

        let error = grant(&mut conn, task).await.unwrap_err();

        assert!(
            matches!(error, PersistenceError::JobNotInState { .. }),
            "{state}"
        );
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_leased_task_cannot_be_granted_again(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    let task = plan(&mut conn, id, 1).await[0];
    force_job_state(&mut conn, id, JobState::Running).await;
    grant(&mut conn, task).await.unwrap();

    let error = grant(&mut conn, task).await.unwrap_err();

    assert!(matches!(error, PersistenceError::TaskTransition(_)));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_schema_allows_one_active_attempt_per_task(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    let task = plan(&mut conn, id, 1).await[0];
    force_job_state(&mut conn, id, JobState::Running).await;
    grant(&mut conn, task).await.unwrap();

    let error = sqlx::query(
        "INSERT INTO attempts (id, task_id, generation, state, worker_session_id, lease_expires_at)
         VALUES (gen_random_uuid(), $1, 2, 'Running', gen_random_uuid(), now() + interval '1 minute')",
    )
    .bind(task.as_uuid())
    .execute(&pool)
    .await
    .unwrap_err();

    assert_eq!(sqlstate(&error), "23505");
    assert_eq!(
        error.as_database_error().and_then(|e| e.constraint()),
        Some("attempts_one_active_per_task")
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_failed_attempt_waits_to_retry_with_a_new_generation(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    let task = plan(&mut conn, id, 1).await[0];
    force_job_state(&mut conn, id, JobState::Running).await;
    let first = grant(&mut conn, task).await.unwrap();

    let ended = attempts::end_attempt(&mut conn, &fail_for_retry(&first))
        .await
        .unwrap();
    let waiting = plans::task(&mut conn, task).await.unwrap();
    release_task(&mut conn, task).await;
    let second = grant(&mut conn, task).await.unwrap();

    assert_eq!(ended.state, AttemptState::Failed);
    assert!(ended.finished_at.is_some());
    assert_eq!(waiting.state, TaskState::RetryWaiting);
    assert!(waiting.retry_not_before.is_some());
    assert_eq!(second.generation, 2);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_task_gets_at_most_three_attempts(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    let task = plan(&mut conn, id, 1).await[0];
    force_job_state(&mut conn, id, JobState::Running).await;
    for _ in 0..3 {
        let attempt = grant(&mut conn, task).await.unwrap();
        attempts::end_attempt(&mut conn, &fail_for_retry(&attempt))
            .await
            .unwrap();
        release_task(&mut conn, task).await;
    }

    let error = grant(&mut conn, task).await.unwrap_err();

    assert!(matches!(error, PersistenceError::RetryBudgetExhausted { task: t } if t == task));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn ending_an_attempt_checks_generation_and_lifecycle(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    let task = plan(&mut conn, id, 1).await[0];
    force_job_state(&mut conn, id, JobState::Running).await;
    let attempt = grant(&mut conn, task).await.unwrap();

    let mut stale = fail_for_retry(&attempt);
    stale.generation = 2;
    let mut accepted = fail_for_retry(&attempt);
    accepted.attempt_state = AttemptState::Accepted;
    let mut no_retry_time = fail_for_retry(&attempt);
    no_retry_time.retry_not_before = None;

    assert!(matches!(
        attempts::end_attempt(&mut conn, &stale).await.unwrap_err(),
        PersistenceError::StaleAttempt { .. }
    ));
    assert!(matches!(
        attempts::end_attempt(&mut conn, &accepted)
            .await
            .unwrap_err(),
        PersistenceError::AttemptTransition(_)
    ));
    assert_eq!(
        constraint(
            &attempts::end_attempt(&mut conn, &no_retry_time)
                .await
                .unwrap_err()
        ),
        "tasks_check"
    );
    // A rejected ending changes nothing; the attempt can still end once.
    attempts::end_attempt(&mut conn, &fail_for_retry(&attempt))
        .await
        .unwrap();
    assert!(matches!(
        attempts::end_attempt(&mut conn, &fail_for_retry(&attempt))
            .await
            .unwrap_err(),
        PersistenceError::AttemptTransition(_)
    ));
}

// Receipts

fn outcome(acceptance: Acceptance) -> AcceptOutcome {
    acceptance.outcome
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_receipt_waiting_for_the_job_lock_rechecks_lease_expiry(pool: PgPool) {
    use sqlx::Connection;
    let (job_id, attempt) = running_attempt(&pool, "acme").await;
    let mut blocker = pool.acquire().await.unwrap();
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
    sqlx::query("SELECT id FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(job_id.as_uuid())
        .execute(&mut *tx)
        .await
        .unwrap();
    let submission = receipt(&attempt, 'a');
    let (accepted, ()) = tokio::join!(attempts::accept_receipt(&mut waiter, &submission), async {
        support::wait_until_blocked(&mut tx, waiter_pid, blocker_pid).await;
        // Put expiry strictly after the waiter's transaction began and before
        // it resumes. No sleeps or wall-clock ordering assumptions are needed.
        let expired: bool = sqlx::query_scalar(
                "UPDATE attempts SET lease_expires_at = s.xact_start + (clock_timestamp() - s.xact_start) / 2
                 FROM pg_stat_activity s WHERE attempts.id = $1 AND s.pid = $2
                 RETURNING lease_expires_at > s.xact_start AND lease_expires_at < clock_timestamp()"
            ).bind(attempt.id.as_uuid()).bind(waiter_pid).fetch_one(&mut *tx).await.unwrap();
        assert!(expired);
        tx.commit().await.unwrap();
    });
    assert_eq!(accepted.unwrap().outcome, AcceptOutcome::StaleAttempt);
    assert_eq!(
        attempts::attempt(&mut waiter, attempt.id)
            .await
            .unwrap()
            .state,
        AttemptState::Granted
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM receipts")
        .fetch_one(&mut *waiter)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_authoritative_receipt_is_accepted_once(pool: PgPool) {
    let (_, attempt) = running_attempt(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();

    let first = attempts::accept_receipt(&mut conn, &receipt(&attempt, '1'))
        .await
        .unwrap();
    let replay = attempts::accept_receipt(&mut conn, &receipt(&attempt, '1'))
        .await
        .unwrap();
    let task = plans::task(&mut conn, attempt.task_id).await.unwrap();
    let accepted = attempts::attempt(&mut conn, attempt.id).await.unwrap();

    assert_eq!(
        first,
        Acceptance {
            outcome: AcceptOutcome::Accepted,
            job_state: JobState::Running
        }
    );
    assert_eq!(outcome(replay), AcceptOutcome::AlreadyAccepted);
    assert_eq!(task.state, TaskState::Accepted);
    assert_eq!(accepted.state, AttemptState::Accepted);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_replay_with_different_content_is_a_conflict(pool: PgPool) {
    let (_, attempt) = running_attempt(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    attempts::accept_receipt(&mut conn, &receipt(&attempt, '1'))
        .await
        .unwrap();

    let error = attempts::accept_receipt(&mut conn, &receipt(&attempt, '2'))
        .await
        .unwrap_err();

    assert!(matches!(error, PersistenceError::ReceiptConflict { task } if task == attempt.task_id));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_accepted_replay_survives_later_cancellation(pool: PgPool) {
    let (job, attempt) = running_attempt(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    attempts::accept_receipt(&mut conn, &receipt(&attempt, '1'))
        .await
        .unwrap();
    force_job_state(&mut conn, job, JobState::Cancelled).await;

    let replay = attempts::accept_receipt(&mut conn, &receipt(&attempt, '1'))
        .await
        .unwrap();

    assert_eq!(
        replay,
        Acceptance {
            outcome: AcceptOutcome::AlreadyAccepted,
            job_state: JobState::Cancelled
        }
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_fences_unaccepted_results(pool: PgPool) {
    for (slug, state) in [
        ("acme", JobState::Cancelling),
        ("globex", JobState::Cancelled),
    ] {
        let (job, attempt) = running_attempt(&pool, slug).await;
        let mut conn = pool.acquire().await.unwrap();
        force_job_state(&mut conn, job, state).await;

        let result = attempts::accept_receipt(&mut conn, &receipt(&attempt, '1'))
            .await
            .unwrap();

        assert_eq!(outcome(result), AcceptOutcome::Cancelled, "{state}");
        let task = plans::task(&mut conn, attempt.task_id).await.unwrap();
        assert_eq!(task.state, TaskState::Leased, "{state}");
        sqlx::query("DELETE FROM attempts")
            .execute(&pool)
            .await
            .unwrap();
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn results_from_non_authoritative_attempts_are_stale(pool: PgPool) {
    // Wrong generation.
    let (_, attempt) = running_attempt(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    let mut wrong_generation = receipt(&attempt, '1');
    wrong_generation.generation = 2;
    let result = attempts::accept_receipt(&mut conn, &wrong_generation)
        .await
        .unwrap();
    assert_eq!(outcome(result), AcceptOutcome::StaleAttempt);

    // Lease expired by database time.
    expire_lease(&mut conn, attempt.id).await;
    let result = attempts::accept_receipt(&mut conn, &receipt(&attempt, '1'))
        .await
        .unwrap();
    assert_eq!(outcome(result), AcceptOutcome::StaleAttempt);

    // Attempt already ended.
    let (_, ended) = running_attempt(&pool, "globex").await;
    attempts::end_attempt(&mut conn, &fail_for_retry(&ended))
        .await
        .unwrap();
    let result = attempts::accept_receipt(&mut conn, &receipt(&ended, '1'))
        .await
        .unwrap();
    assert_eq!(outcome(result), AcceptOutcome::StaleAttempt);

    // Job stopped for another reason.
    let (job, fenced) = running_attempt(&pool, "initech").await;
    force_job_state(&mut conn, job, JobState::ReplanRequired).await;
    let result = attempts::accept_receipt(&mut conn, &receipt(&fenced, '1'))
        .await
        .unwrap();
    assert_eq!(
        result,
        Acceptance {
            outcome: AcceptOutcome::StaleAttempt,
            job_state: JobState::ReplanRequired
        }
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_superseded_attempt_cannot_replace_the_accepted_receipt(pool: PgPool) {
    let (_, first) = running_attempt(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    attempts::end_attempt(&mut conn, &fail_for_retry(&first))
        .await
        .unwrap();
    release_task(&mut conn, first.task_id).await;
    let second = grant(&mut conn, first.task_id).await.unwrap();
    attempts::accept_receipt(&mut conn, &receipt(&second, '2'))
        .await
        .unwrap();

    let late = attempts::accept_receipt(&mut conn, &receipt(&first, '1'))
        .await
        .unwrap();

    assert_eq!(outcome(late), AcceptOutcome::StaleAttempt);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_schema_allows_one_receipt_per_task_and_keeps_it_immutable(pool: PgPool) {
    let (_, attempt) = running_attempt(&pool, "acme").await;
    let mut conn = pool.acquire().await.unwrap();
    attempts::accept_receipt(&mut conn, &receipt(&attempt, '1'))
        .await
        .unwrap();

    let duplicate = sqlx::query(
        "INSERT INTO receipts (attempt_id, task_id, content, content_hash)
         SELECT attempt_id, task_id, content, content_hash FROM receipts",
    )
    .execute(&pool)
    .await
    .unwrap_err();
    let update = sqlx::query("UPDATE receipts SET content = '{}'")
        .execute(&pool)
        .await
        .unwrap_err();

    assert_eq!(sqlstate(&duplicate), "23505");
    assert_eq!(sqlstate(&update), "23001");
}

// Publications

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_job_has_at_most_one_publication(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    let insert = "INSERT INTO publications (id, job_id) VALUES ($1, $2)";
    sqlx::query(insert)
        .bind(PublicationId::generate().as_uuid())
        .bind(id.as_uuid())
        .execute(&pool)
        .await
        .unwrap();

    let error = sqlx::query(insert)
        .bind(PublicationId::generate().as_uuid())
        .bind(id.as_uuid())
        .execute(&pool)
        .await
        .unwrap_err();

    assert_eq!(sqlstate(&error), "23505");
    assert_eq!(
        error.as_database_error().and_then(|e| e.constraint()),
        Some("publications_job_id_key")
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_denied_sender_cannot_release_authority_held_by_another(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let epoch = acquire_leadership(&mut conn, "test", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap()
        .epoch;

    // No sender authorized: the denial resolves the job and releases authority.
    let unowned = job(&mut conn, &tenant).await;
    force_job_state(&mut conn, unowned, JobState::Committing).await;
    let resolved = publications::resolve_authorization_denied(&mut conn, unowned, 0, epoch)
        .await
        .unwrap()
        .expect("no sender is authorized, so the denial resolves");
    assert_eq!(resolved.state, JobState::Failed);
    assert_eq!(
        resolved.state_reason.as_deref(),
        Some("AuthorizationRevoked")
    );

    // Another sender authorized at the same revision: the denial must neither
    // resolve the job nor release the barrier, because send authorization does
    // not bump the revision.
    let sent = job(&mut conn, &tenant).await;
    force_job_state(&mut conn, sent, JobState::Committing).await;
    sqlx::query("UPDATE jobs SET publication_started_at = clock_timestamp() WHERE id = $1")
        .bind(sent.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let denied = publications::resolve_authorization_denied(&mut conn, sent, 0, epoch)
        .await
        .unwrap();
    assert!(
        denied.is_none(),
        "a competing authorized sender owns the outcome"
    );
    let current = jobs::job(&mut conn, tenant.organization, sent)
        .await
        .unwrap();
    assert_eq!(current.state, JobState::Committing);
    assert!(current.publication_started_at.is_some());
}
