//! Cancellation requests and the send-authorization boundary against real
//! PostgreSQL (BS-003.T5).

mod support;

use bergsmith_common::ids::JobId;
use bergsmith_common::job::{JobState, JobTransitionError};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::cancellation::{
    self, CancelJob, CancellationDisposition, SendAuthorization,
};
use bergsmith_persistence::jobs::{self, JobEventKind, Transition};
use sqlx::PgPool;
use support::{force_job_state, job, leader, sqlstate, tenant};

fn cancel(job_id: JobId, reason: &str) -> CancelJob {
    CancelJob {
        job_id,
        requested_by: None,
        reason: Some(reason.to_owned()),
    }
}

/// Authorizes at revision 0 under epoch 1, the first leader in a fresh database.
fn authorize(job_id: JobId) -> SendAuthorization {
    SendAuthorization {
        job_id,
        expected_revision: 0,
        coordinator_epoch: 1,
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_before_send_authorization_moves_the_job_to_cancelling(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    assert_eq!(leader(&mut conn).await, 1);

    for state in [
        JobState::Planning,
        JobState::Queued,
        JobState::Running,
        JobState::ReadyToCommit,
        JobState::Committing,
    ] {
        let id = job(&mut conn, &tenant).await;
        force_job_state(&mut conn, id, state).await;
        let request = CancelJob {
            job_id: id,
            requested_by: Some(tenant.user),
            reason: Some("operator".to_owned()),
        };

        let outcome = cancellation::request_cancellation(&mut conn, &request)
            .await
            .unwrap();

        assert_eq!(
            outcome.disposition,
            CancellationDisposition::Cancelling,
            "{state}"
        );
        assert_eq!(outcome.job.state, JobState::Cancelling, "{state}");
        assert_eq!(outcome.job.state_revision, 1, "{state}");
        let recorded = outcome.job.cancellation.unwrap();
        assert_eq!(recorded.requested_by, Some(tenant.user));
        assert_eq!(recorded.reason.as_deref(), Some("operator"));
        let kinds: Vec<_> = jobs::job_events(&mut conn, id)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                JobEventKind::Created,
                JobEventKind::CancellationRequested,
                JobEventKind::Transitioned
            ],
            "{state}"
        );
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_requests_are_idempotent_and_keep_the_first_request(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    assert_eq!(leader(&mut conn).await, 1);
    let id = job(&mut conn, &tenant).await;
    let first = cancellation::request_cancellation(&mut conn, &cancel(id, "first"))
        .await
        .unwrap();

    let second = cancellation::request_cancellation(&mut conn, &cancel(id, "second"))
        .await
        .unwrap();

    assert_eq!(second.disposition, CancellationDisposition::Cancelling);
    assert_eq!(second.job, first.job);
    assert_eq!(
        second.job.cancellation.unwrap().reason.as_deref(),
        Some("first")
    );
    assert_eq!(jobs::job_events(&mut conn, id).await.unwrap().len(), 3);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_after_send_authorization_is_too_late(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    assert_eq!(leader(&mut conn).await, 1);
    let id = job(&mut conn, &tenant).await;
    force_job_state(&mut conn, id, JobState::Committing).await;
    cancellation::authorize_send(&mut conn, authorize(id))
        .await
        .unwrap();

    let outcome = cancellation::request_cancellation(&mut conn, &cancel(id, "late"))
        .await
        .unwrap();

    assert_eq!(
        outcome.disposition,
        CancellationDisposition::TooLatePublicationStarted
    );
    assert_eq!(outcome.job.state, JobState::Committing);
    // Recorded for audit and to suppress replays.
    assert!(outcome.job.cancellation.is_some());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_of_an_unknown_outcome_is_recorded_but_unresolved(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    assert_eq!(leader(&mut conn).await, 1);
    let id = job(&mut conn, &tenant).await;
    force_job_state(&mut conn, id, JobState::CommitOutcomeUnknown).await;

    let outcome = cancellation::request_cancellation(&mut conn, &cancel(id, "audit"))
        .await
        .unwrap();

    assert_eq!(
        outcome.disposition,
        CancellationDisposition::PublicationUnresolved
    );
    assert_eq!(outcome.job.state, JobState::CommitOutcomeUnknown);
    assert!(outcome.job.cancellation.is_some());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_never_modifies_terminal_jobs(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    assert_eq!(leader(&mut conn).await, 1);

    for terminal in JobState::ALL.into_iter().filter(|s| s.is_terminal()) {
        let id = job(&mut conn, &tenant).await;
        force_job_state(&mut conn, id, terminal).await;

        let outcome = cancellation::request_cancellation(&mut conn, &cancel(id, "late"))
            .await
            .unwrap();

        let expected = if terminal == JobState::Succeeded {
            CancellationDisposition::TooLateCommitted
        } else {
            CancellationDisposition::AlreadyEnded(terminal)
        };
        assert_eq!(outcome.disposition, expected, "{terminal}");
        assert_eq!(outcome.job.cancellation, None, "{terminal}");
        assert_eq!(jobs::job_events(&mut conn, id).await.unwrap().len(), 1);
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn send_authorization_records_the_boundary_without_a_new_revision(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    assert_eq!(leader(&mut conn).await, 1);
    let id = job(&mut conn, &tenant).await;
    force_job_state(&mut conn, id, JobState::Committing).await;

    let job = cancellation::authorize_send(&mut conn, authorize(id))
        .await
        .unwrap();
    let last = jobs::job_events(&mut conn, id)
        .await
        .unwrap()
        .pop()
        .unwrap();

    assert!(job.publication_started_at.is_some());
    assert_eq!(job.state, JobState::Committing);
    assert_eq!(job.state_revision, 0);
    assert_eq!(last.kind, JobEventKind::PublicationAuthorized);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn send_authorization_checks_revision_state_and_prior_authorization(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    assert_eq!(leader(&mut conn).await, 1);
    let ready = job(&mut conn, &tenant).await;
    force_job_state(&mut conn, ready, JobState::ReadyToCommit).await;
    let committing = job(&mut conn, &tenant).await;
    force_job_state(&mut conn, committing, JobState::Committing).await;

    let wrong_state = cancellation::authorize_send(&mut conn, authorize(ready))
        .await
        .unwrap_err();
    let stale = cancellation::authorize_send(
        &mut conn,
        SendAuthorization {
            job_id: committing,
            expected_revision: 7,
            coordinator_epoch: 1,
        },
    )
    .await
    .unwrap_err();
    cancellation::authorize_send(&mut conn, authorize(committing))
        .await
        .unwrap();
    let twice = cancellation::authorize_send(&mut conn, authorize(committing))
        .await
        .unwrap_err();

    assert!(matches!(
        wrong_state,
        PersistenceError::JobNotInState { .. }
    ));
    assert!(matches!(stale, PersistenceError::RevisionConflict { .. }));
    assert!(matches!(
        twice,
        PersistenceError::PublicationAlreadyAuthorized { .. }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn send_authorization_is_refused_after_cancellation(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    assert_eq!(leader(&mut conn).await, 1);
    let id = job(&mut conn, &tenant).await;
    force_job_state(&mut conn, id, JobState::Committing).await;
    cancellation::request_cancellation(&mut conn, &cancel(id, "stop"))
        .await
        .unwrap();

    let error = cancellation::authorize_send(
        &mut conn,
        SendAuthorization {
            job_id: id,
            expected_revision: 1,
            coordinator_epoch: 1,
        },
    )
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        PersistenceError::JobNotInState {
            state: JobState::Cancelling,
            ..
        }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_and_send_authorization_race_to_one_consistent_outcome(pool: PgPool) {
    let mut setup = pool.acquire().await.unwrap();
    let tenant = tenant(&mut setup, "acme").await;
    assert_eq!(leader(&mut setup).await, 1);
    let id = job(&mut setup, &tenant).await;
    force_job_state(&mut setup, id, JobState::Committing).await;
    drop(setup);

    let mut canceller = pool.acquire().await.unwrap();
    let mut publisher = pool.acquire().await.unwrap();
    let request = cancel(id, "race");
    let (cancelled, authorized) = tokio::join!(
        cancellation::request_cancellation(&mut canceller, &request),
        cancellation::authorize_send(&mut publisher, authorize(id)),
    );

    let disposition = cancelled.unwrap().disposition;
    match disposition {
        CancellationDisposition::Cancelling => assert!(authorized.is_err()),
        CancellationDisposition::TooLatePublicationStarted => assert!(authorized.is_ok()),
        other => panic!("unexpected disposition {other:?}"),
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn after_send_authorization_endings_need_proof_and_cancelling_is_closed(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    assert_eq!(leader(&mut conn).await, 1);
    let id = job(&mut conn, &tenant).await;
    force_job_state(&mut conn, id, JobState::Committing).await;
    cancellation::authorize_send(&mut conn, authorize(id))
        .await
        .unwrap();
    let to = |state| Transition {
        job_id: id,
        expected_revision: 0,
        to: state,
        reason: None,
        actor: None,
        non_publication_proven: false,
    };

    let cancelling = jobs::transition_job(&mut conn, &to(JobState::Cancelling))
        .await
        .unwrap_err();
    let unproven = jobs::transition_job(&mut conn, &to(JobState::Failed))
        .await
        .unwrap_err();
    let succeeded = jobs::transition_job(&mut conn, &to(JobState::Succeeded))
        .await
        .unwrap();

    assert!(matches!(
        cancelling,
        PersistenceError::JobTransition(
            JobTransitionError::CancellationAfterSendAuthorization { .. }
        )
    ));
    assert!(matches!(
        unproven,
        PersistenceError::JobTransition(JobTransitionError::PublicationNotRuledOut { .. })
    ));
    assert!(succeeded.publication_started_at.is_some());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_schema_keeps_boundaries_monotonic(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    assert_eq!(leader(&mut conn).await, 1);
    let authorized = job(&mut conn, &tenant).await;
    force_job_state(&mut conn, authorized, JobState::Committing).await;
    cancellation::authorize_send(&mut conn, authorize(authorized))
        .await
        .unwrap();
    let cancelled = job(&mut conn, &tenant).await;
    cancellation::request_cancellation(&mut conn, &cancel(cancelled, "stop"))
        .await
        .unwrap();

    let clear_start = sqlx::query("UPDATE jobs SET publication_started_at = NULL WHERE id = $1")
        .bind(authorized.as_uuid())
        .execute(&pool)
        .await
        .unwrap_err();
    let edit_reason = sqlx::query("UPDATE jobs SET cancellation_reason = 'edited' WHERE id = $1")
        .bind(cancelled.as_uuid())
        .execute(&pool)
        .await
        .unwrap_err();
    let start_while_cancelling =
        sqlx::query("UPDATE jobs SET publication_started_at = now() WHERE id = $1")
            .bind(cancelled.as_uuid())
            .execute(&pool)
            .await
            .unwrap_err();

    assert_eq!(sqlstate(&clear_start), "23001");
    assert_eq!(sqlstate(&edit_reason), "23001");
    assert_eq!(sqlstate(&start_while_cancelling), "23514");
}
