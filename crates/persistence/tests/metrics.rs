//! Ledger metric aggregates and worker session listing (BS-020.T2).
#![expect(clippy::unwrap_used, reason = "fixtures fail on setup errors")]

mod support;

use std::time::Duration;

use bergsmith_common::ids::{PlanId, TaskId, WorkerSessionId};
use bergsmith_common::job::JobState;
use bergsmith_persistence::metrics::{self, LedgerSnapshot};
use bergsmith_persistence::plans::{self, NewPlan};
use bergsmith_persistence::workers::{self, SessionStatus};
use sqlx::{PgConnection, PgPool};

const TIMEOUT: Duration = Duration::from_secs(5);

async fn session(
    conn: &mut PgConnection,
    organization: bergsmith_common::ids::OrganizationId,
    name: &str,
) -> WorkerSessionId {
    let principal = support::service_principal(conn, organization, name).await;
    let id = WorkerSessionId::generate();
    workers::register(
        conn,
        &workers::Identity {
            organization,
            principal,
            pool: "native".into(),
        },
        &workers::Registration {
            id,
            worker_name: name.into(),
            capabilities: vec!["native-v1".into()],
            memory_bytes: 1024,
            cpu_millis: 1000,
            build_version: "test".into(),
        },
        Duration::from_secs(90),
    )
    .await
    .unwrap();
    id
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_empty_ledger_reports_zeroes(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();

    let snapshot = metrics::ledger_snapshot(&mut conn, TIMEOUT).await.unwrap();

    assert_eq!(snapshot, LedgerSnapshot::default());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_snapshot_counts_jobs_tasks_plans_cancellations_and_sessions(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;

    let planned = support::job(&mut conn, &tenant).await;
    plans::create_plan(
        &mut conn,
        &NewPlan {
            id: PlanId::generate(),
            job_id: planned,
            content: serde_json::json!({ "estimate": { "bytes": 1234 } }),
            content_hash: support::digest('b'),
            task_ids: vec![TaskId::generate(), TaskId::generate()],
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE tasks SET updated_at = now() - interval '90 seconds'")
        .execute(&mut *conn)
        .await
        .unwrap();
    let failed = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, failed, JobState::Failed).await;
    let late = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, late, JobState::CommitOutcomeUnknown).await;
    sqlx::query(
        "UPDATE jobs SET publication_started_at = now() - interval '10 minutes',
                         cancellation_requested_at = now() - interval '5 minutes'
         WHERE id = $1",
    )
    .bind(late.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    session(&mut conn, tenant.organization, "worker-1").await;

    let snapshot = metrics::ledger_snapshot(&mut conn, TIMEOUT).await.unwrap();

    assert_eq!(snapshot.ended_jobs.get("Failed"), Some(&1));
    assert_eq!(snapshot.active_jobs.get("CommitOutcomeUnknown"), Some(&1));
    assert_eq!(snapshot.active_jobs.values().sum::<i64>(), 2);
    assert_eq!(snapshot.pending_tasks, 2);
    assert!(snapshot.oldest_pending_task_seconds >= 90.0);
    assert_eq!(snapshot.planned_input_bytes, 1234);
    assert_eq!(snapshot.unresolved_publications, 1);
    assert!(snapshot.oldest_unresolved_publication_seconds >= 600.0);
    assert_eq!(snapshot.cancellation_requests, 1);
    assert_eq!(snapshot.cancellation_requests_too_late, 1);
    assert_eq!(snapshot.worker_sessions.get("live"), Some(&1));
    assert_eq!(snapshot.running_attempts, 0);
    assert!(snapshot.schedule_lag_seconds.abs() < f64::EPSILON);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_statement_over_the_timeout_fails_the_snapshot(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut blocker = pool.acquire().await.unwrap();
    let mut lock = sqlx::Connection::begin(&mut *blocker).await.unwrap();
    sqlx::query("LOCK TABLE plans IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();

    let error = metrics::ledger_snapshot(&mut conn, Duration::from_millis(200))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        bergsmith_persistence::PersistenceError::Database(_)
    ));
    lock.rollback().await.unwrap();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn sessions_are_listed_per_organization_with_status_and_cursor(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other = support::tenant(&mut conn, "other").await;
    let first = session(&mut conn, tenant.organization, "worker-1").await;
    let second = session(&mut conn, tenant.organization, "worker-2").await;
    session(&mut conn, other.organization, "worker-3").await;
    sqlx::query(
        "UPDATE worker_sessions SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(second.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("UPDATE worker_sessions SET draining = true WHERE id = $1")
        .bind(first.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();

    let listed = workers::list_sessions(&mut conn, tenant.organization, None, 10)
        .await
        .unwrap();

    let mut expected = [first, second];
    expected.sort_by_key(|id| *id.as_uuid());
    assert_eq!(
        listed.iter().map(|s| s.id).collect::<Vec<_>>(),
        expected.to_vec()
    );
    let status = |id| listed.iter().find(|s| s.id == id).unwrap().status;
    assert_eq!(status(first), SessionStatus::Draining);
    assert_eq!(status(second), SessionStatus::Expired);
    assert!(listed.iter().all(|s| s.running_attempt_id.is_none()));
    assert_eq!(listed[0].pool, "native");
    assert_eq!(listed[0].capabilities, vec!["native-v1".to_owned()]);

    let rest = workers::list_sessions(&mut conn, tenant.organization, Some(expected[0]), 10)
        .await
        .unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].id, expected[1]);
    assert_eq!(SessionStatus::Live.as_str(), "live");
}
