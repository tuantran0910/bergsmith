//! Coordinator leadership lease and epoch against real PostgreSQL (BS-003.T6).

mod support;

use std::time::Duration;

use bergsmith_common::job::JobState;
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::cancellation::{self, SendAuthorization};
use bergsmith_persistence::leadership::{acquire_leadership, release_leadership, renew_leadership};
use sqlx::PgPool;
use support::{force_job_state, job, tenant};

const LEASE: Duration = Duration::from_secs(60);

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_first_coordinator_takes_epoch_one(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();

    let leadership = acquire_leadership(&mut conn, "a", LEASE)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(leadership.holder, "a");
    assert_eq!(leadership.epoch, 1);
    assert!(leadership.lease_expires_at > leadership.acquired_at);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_live_lease_blocks_every_other_acquisition(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    acquire_leadership(&mut conn, "a", LEASE)
        .await
        .unwrap()
        .unwrap();

    let other = acquire_leadership(&mut conn, "b", LEASE).await.unwrap();
    let same_holder = acquire_leadership(&mut conn, "a", LEASE).await.unwrap();

    assert_eq!(other, None);
    assert_eq!(
        same_holder, None,
        "a restarted process must not inherit an epoch"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_expired_lease_passes_to_a_new_holder_with_a_larger_epoch(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    acquire_leadership(&mut conn, "a", Duration::ZERO)
        .await
        .unwrap()
        .unwrap();

    let successor = acquire_leadership(&mut conn, "b", LEASE)
        .await
        .unwrap()
        .unwrap();
    let old_renewal = renew_leadership(&mut conn, "a", 1, LEASE)
        .await
        .unwrap_err();

    assert_eq!(successor.epoch, 2);
    assert!(matches!(
        old_renewal,
        PersistenceError::LeadershipLost { epoch: 1 }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_holder_renews_its_lease_under_the_same_epoch(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acquired = acquire_leadership(&mut conn, "a", Duration::from_secs(1))
        .await
        .unwrap()
        .unwrap();

    let renewed = renew_leadership(&mut conn, "a", 1, LEASE).await.unwrap();
    let wrong_epoch = renew_leadership(&mut conn, "a", 2, LEASE)
        .await
        .unwrap_err();

    assert_eq!(renewed.epoch, 1);
    assert!(renewed.lease_expires_at > acquired.lease_expires_at);
    assert!(matches!(
        wrong_epoch,
        PersistenceError::LeadershipLost { epoch: 2 }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn releasing_hands_over_leadership_immediately(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    acquire_leadership(&mut conn, "a", LEASE)
        .await
        .unwrap()
        .unwrap();

    release_leadership(&mut conn, "a", 1).await.unwrap();
    let successor = acquire_leadership(&mut conn, "b", LEASE)
        .await
        .unwrap()
        .unwrap();
    // Releasing a lost lease is harmless and leaves the new holder alone.
    release_leadership(&mut conn, "a", 1).await.unwrap();
    let renewed = renew_leadership(&mut conn, "b", 2, LEASE).await.unwrap();

    assert_eq!(successor.epoch, 2);
    assert_eq!(renewed.holder, "b");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn concurrent_candidates_elect_exactly_one_leader(pool: PgPool) {
    let mut first = pool.acquire().await.unwrap();
    let mut second = pool.acquire().await.unwrap();

    let (a, b) = tokio::join!(
        acquire_leadership(&mut first, "a", LEASE),
        acquire_leadership(&mut second, "b", LEASE),
    );

    let leaders = [a.unwrap(), b.unwrap()].into_iter().flatten().count();
    assert_eq!(leaders, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn send_authorization_requires_live_leadership(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    force_job_state(&mut conn, id, JobState::Committing).await;
    let authorize = |epoch| SendAuthorization {
        job_id: id,
        expected_revision: 0,
        coordinator_epoch: epoch,
    };

    let no_leader = cancellation::authorize_send(&mut conn, authorize(1))
        .await
        .unwrap_err();
    acquire_leadership(&mut conn, "a", Duration::ZERO)
        .await
        .unwrap()
        .unwrap();
    let expired = cancellation::authorize_send(&mut conn, authorize(1))
        .await
        .unwrap_err();
    acquire_leadership(&mut conn, "b", LEASE)
        .await
        .unwrap()
        .unwrap();
    let superseded = cancellation::authorize_send(&mut conn, authorize(1))
        .await
        .unwrap_err();
    let current = cancellation::authorize_send(&mut conn, authorize(2))
        .await
        .unwrap();

    for error in [no_leader, expired, superseded] {
        assert!(matches!(
            error,
            PersistenceError::LeadershipLost { epoch: 1 }
        ));
    }
    assert!(current.publication_started_at.is_some());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn holders_must_be_named(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();

    let error = acquire_leadership(&mut conn, "", LEASE).await.unwrap_err();

    assert!(matches!(error, PersistenceError::InvalidValue { .. }));
}
