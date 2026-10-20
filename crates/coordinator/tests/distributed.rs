//! BS-016: process crashes, lost responses, and the 100-group M3 release gate.
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "acceptance setup and assertions fail immediately"
)]

#[path = "support/distributed/mod.rs"]
mod distributed;
#[path = "../../persistence/tests/support/mod.rs"]
mod support;

use distributed::Harness;
use sqlx::PgPool;

// The same integration-test executable hosts an isolated coordinator process.
// Only its parent sets the config path; this entry point is not an acceptance gate.
#[tokio::test]
async fn coordinator_process() {
    if let Some(path) = std::env::var_os("BERGSMITH_RECOVERY_CHILD") {
        Box::pin(distributed::coordinator(std::path::Path::new(&path))).await;
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn lost_grants_worker_death_and_lost_receipts_survive_coordinator_restart(pool: PgPool) {
    let mut run = Harness::new(pool, 6).await;
    run.exercise_failures().await;
    run.assert_execution().await;
    run.publish_and_recover().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn one_hundred_groups_across_three_workers_publish_once_after_process_failures(pool: PgPool) {
    let mut run = Harness::new(pool, 100).await;
    run.exercise_failures().await;
    run.assert_execution().await;
    run.publish_and_recover().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn pending_cancellation_after_intent_survives_process_restart_without_publication(
    pool: PgPool,
) {
    let mut run = Harness::new(pool, 6).await;
    run.exercise_failures().await;
    run.assert_execution().await;
    run.cancel_intent_and_recover().await;
    run.assert_fresh_retry_plan().await;
}
