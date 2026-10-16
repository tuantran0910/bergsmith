//! Revision-checked job transitions against real PostgreSQL (BS-003.T4).

mod support;

use bergsmith_common::ids::JobId;
use bergsmith_common::job::{JobState, JobTransitionError};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::jobs::{self, JobEventKind, Transition};
use sqlx::PgPool;
use support::{force_job_state, job, tenant};

fn to(job_id: JobId, expected_revision: i64, state: JobState) -> Transition {
    Transition {
        job_id,
        expected_revision,
        to: state,
        reason: None,
        actor: None,
        non_publication_proven: false,
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_transition_bumps_the_revision_and_records_an_event(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;

    let mut transition = to(id, 0, JobState::Queued);
    transition.reason = Some("PlanPersisted".to_owned());
    transition.actor = Some(tenant.user);
    let job = jobs::transition_job(&mut conn, &transition).await.unwrap();
    let events = jobs::job_events(&mut conn, id).await.unwrap();

    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.state_revision, 1);
    assert_eq!(job.state_reason.as_deref(), Some("PlanPersisted"));
    let last = events.last().unwrap();
    assert_eq!(last.kind, JobEventKind::Transitioned);
    assert_eq!(last.from_state, Some(JobState::Planning));
    assert_eq!(last.to_state, JobState::Queued);
    assert_eq!(last.state_revision, 1);
    assert_eq!(last.reason.as_deref(), Some("PlanPersisted"));
    assert_eq!(last.actor, Some(tenant.user));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_full_lifecycle_advances_one_revision_per_step(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    let path = [
        JobState::Queued,
        JobState::Running,
        JobState::ReadyToCommit,
        JobState::Committing,
        JobState::Succeeded,
    ];

    for (revision, state) in (0..).zip(path) {
        jobs::transition_job(&mut conn, &to(id, revision, state))
            .await
            .unwrap();
    }

    let job = jobs::job(&mut conn, tenant.organization, id).await.unwrap();
    let events = jobs::job_events(&mut conn, id).await.unwrap();
    assert_eq!(job.state, JobState::Succeeded);
    assert_eq!(job.state_revision, 5);
    assert_eq!(events.len(), 6);
    assert_eq!(
        events.iter().map(|e| e.state_revision).collect::<Vec<_>>(),
        [0, 1, 2, 3, 4, 5]
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_stale_revision_is_rejected_without_changes(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    jobs::transition_job(&mut conn, &to(id, 0, JobState::Queued))
        .await
        .unwrap();

    let error = jobs::transition_job(&mut conn, &to(id, 0, JobState::Cancelling))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        PersistenceError::RevisionConflict {
            expected: 0,
            actual: 1,
            ..
        }
    ));
    let job = jobs::job(&mut conn, tenant.organization, id).await.unwrap();
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(jobs::job_events(&mut conn, id).await.unwrap().len(), 2);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn concurrent_writers_at_the_same_revision_have_one_winner(pool: PgPool) {
    let mut setup = pool.acquire().await.unwrap();
    let tenant = tenant(&mut setup, "acme").await;
    let id = job(&mut setup, &tenant).await;
    drop(setup);

    let mut first = pool.acquire().await.unwrap();
    let mut second = pool.acquire().await.unwrap();
    let queue = to(id, 0, JobState::Queued);
    let cancel = to(id, 0, JobState::Cancelling);
    let (a, b) = tokio::join!(
        jobs::transition_job(&mut first, &queue),
        jobs::transition_job(&mut second, &cancel),
    );

    let results = [a, b];
    let winners = results.iter().filter(|r| r.is_ok()).count();
    let conflicts = results
        .iter()
        .filter(|r| matches!(r, Err(PersistenceError::RevisionConflict { .. })))
        .count();
    assert_eq!((winners, conflicts), (1, 1));
    let mut conn = pool.acquire().await.unwrap();
    let job = jobs::job(&mut conn, tenant.organization, id).await.unwrap();
    assert_eq!(job.state_revision, 1);
    assert_eq!(jobs::job_events(&mut conn, id).await.unwrap().len(), 2);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn transitions_outside_the_lifecycle_are_rejected(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;

    let error = jobs::transition_job(&mut conn, &to(id, 0, JobState::Committing))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        PersistenceError::JobTransition(JobTransitionError::NotAllowed {
            from: JobState::Planning,
            to: JobState::Committing,
        })
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn terminal_jobs_never_transition(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;

    for terminal in JobState::ALL.into_iter().filter(|s| s.is_terminal()) {
        let id = job(&mut conn, &tenant).await;
        force_job_state(&mut conn, id, terminal).await;

        let error = jobs::transition_job(&mut conn, &to(id, 0, JobState::Queued))
            .await
            .unwrap_err();

        assert!(
            matches!(
                error,
                PersistenceError::JobTransition(JobTransitionError::FromTerminal { .. })
            ),
            "{terminal}"
        );
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unknown_commit_outcome_needs_proof_before_failing(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    force_job_state(&mut conn, id, JobState::CommitOutcomeUnknown).await;

    let unproven = jobs::transition_job(&mut conn, &to(id, 0, JobState::Failed))
        .await
        .unwrap_err();
    let mut proven = to(id, 0, JobState::Failed);
    proven.non_publication_proven = true;
    let failed = jobs::transition_job(&mut conn, &proven).await.unwrap();

    assert!(matches!(
        unproven,
        PersistenceError::JobTransition(JobTransitionError::PublicationNotRuledOut { .. })
    ));
    assert_eq!(failed.state, JobState::Failed);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn transitioning_a_missing_job_is_not_found(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();

    let error = jobs::transition_job(&mut conn, &to(JobId::generate(), 0, JobState::Queued))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        PersistenceError::NotFound { entity: "job", .. }
    ));
}
