//! Snapshot expiration plans and the ledger's protected snapshots (BS-031.T1).
#![expect(clippy::expect_used, reason = "test fixtures fail on setup errors")]

mod support;

use bergsmith_common::ids::{JobId, PlanId};
use bergsmith_common::job::{JobOperation, JobState};
use std::time::Duration;

use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::expirations::{self, NewExpirationPlan};
use bergsmith_persistence::jobs::{self, NewJob};
use bergsmith_persistence::leadership::acquire_leadership;
use bergsmith_persistence::plans::{self, NewPlan};
use sqlx::{Acquire, PgConnection, PgPool};
use support::{Tenant, digest, force_job_state, job, tenant, wait_until_blocked};
use uuid::Uuid;

/// Creates a `Planning` job for `operation` on the tenant's table.
async fn new_job(conn: &mut PgConnection, tenant: &Tenant, operation: JobOperation) -> JobId {
    let id = JobId::generate();
    jobs::create_job(
        conn,
        &NewJob {
            id,
            organization_id: tenant.organization,
            table_id: tenant.table,
            operation,
            retry_of: None,
            created_by: tenant.user,
        },
    )
    .await
    .expect("job is created");
    id
}

/// Persists a taskless plan whose baseline names `snapshot`.
async fn plan_at(conn: &mut PgConnection, job: JobId, snapshot: i64) {
    plans::create_plan(
        conn,
        &NewPlan {
            id: PlanId::generate(),
            job_id: job,
            content: serde_json::json!({
                "baseline": { "snapshot_id": snapshot.to_string() },
                "groups": 0,
            }),
            content_hash: digest('a'),
            task_ids: Vec::new(),
            execution_budget: Duration::from_secs(180),
        },
    )
    .await
    .expect("plan is created");
}

/// Persists a publication intent with `content` for `job`.
async fn intent(conn: &mut PgConnection, job: JobId, content: serde_json::Value) {
    let publication = Uuid::now_v7();
    sqlx::query("INSERT INTO publications (id, job_id) VALUES ($1, $2)")
        .bind(publication)
        .bind(job.as_uuid())
        .execute(&mut *conn)
        .await
        .expect("publication is created");
    sqlx::query(
        "INSERT INTO publication_intents
             (job_id, publication_id, content, request_body, request_hash)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(job.as_uuid())
    .bind(publication)
    .bind(&content)
    .bind(Vec::<u8>::new())
    .bind(digest('b'))
    .execute(&mut *conn)
    .await
    .expect("publication intent is created");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn live_plans_pin_their_baseline_until_the_job_ends(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let planned = job(&mut conn, &tenant).await;
    let other = job(&mut conn, &tenant).await;
    assert_eq!(
        jobs::job_by_id(&mut conn, planned).await.unwrap().operation,
        JobOperation::Compact
    );
    plan_at(&mut conn, planned, 11).await;

    let protected =
        expirations::protected_snapshots(&mut conn, tenant.organization, tenant.table, other)
            .await
            .unwrap();
    assert_eq!(protected, [11].into_iter().collect());

    force_job_state(&mut conn, planned, JobState::Failed).await;
    let protected =
        expirations::protected_snapshots(&mut conn, tenant.organization, tenant.table, other)
            .await
            .unwrap();
    assert!(
        protected.is_empty(),
        "a failed job pins nothing: {protected:?}"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn publication_intents_pin_rewrite_and_expiration_baselines(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let rewrite = job(&mut conn, &tenant).await;
    let expiration = new_job(&mut conn, &tenant, JobOperation::ExpireSnapshots).await;
    let target = new_job(&mut conn, &tenant, JobOperation::ExpireSnapshots).await;

    intent(
        &mut conn,
        rewrite,
        serde_json::json!({ "plan": { "baseline": { "snapshot_id": "21" } } }),
    )
    .await;
    intent(
        &mut conn,
        expiration,
        serde_json::json!({
            "baseline_snapshot_id": "22",
            "protected": ["23", "not-a-number", null],
        }),
    )
    .await;

    let protected =
        expirations::protected_snapshots(&mut conn, tenant.organization, tenant.table, target)
            .await
            .unwrap();
    assert_eq!(protected, [21, 22, 23].into_iter().collect());

    // Resolved publications no longer pin their baselines or inherited set.
    force_job_state(&mut conn, rewrite, JobState::Succeeded).await;
    force_job_state(&mut conn, expiration, JobState::Succeeded).await;
    let released =
        expirations::protected_snapshots(&mut conn, tenant.organization, tenant.table, target)
            .await
            .unwrap();
    assert!(
        released.is_empty(),
        "terminal intents release their pins: {released:?}"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn creating_an_expiration_plan_makes_its_job_ready_to_commit(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let expiring = new_job(&mut conn, &tenant, JobOperation::ExpireSnapshots).await;
    let epoch = acquire_leadership(&mut conn, "test", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("leadership is acquired")
        .epoch;
    let other = job(&mut conn, &tenant).await;
    let content = serde_json::json!({
        "baseline": { "snapshot_id": "31" },
        "snapshots": ["32"],
    });
    let hash = digest('c');

    let transitioned = expirations::create_expiration_plan(
        &mut conn,
        &NewExpirationPlan {
            id: PlanId::generate(),
            job_id: expiring,
            content: &content,
            content_hash: &hash,
        },
        0,
        epoch,
    )
    .await
    .unwrap();

    assert_eq!(transitioned.state, JobState::ReadyToCommit);
    assert_eq!(transitioned.state_revision, 1);
    assert_eq!(transitioned.operation, JobOperation::ExpireSnapshots);
    assert_eq!(
        expirations::expiration_plan(&mut conn, expiring)
            .await
            .unwrap(),
        Some((content, hash))
    );
    let protected =
        expirations::protected_snapshots(&mut conn, tenant.organization, tenant.table, other)
            .await
            .unwrap();
    assert_eq!(protected, [31].into_iter().collect());

    let update = sqlx::query("UPDATE expiration_plans SET content_hash = $2 WHERE job_id = $1")
        .bind(expiring.as_uuid())
        .bind(digest('d'))
        .execute(&mut *conn)
        .await;
    assert!(update.is_err(), "expiration plans are immutable");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn creating_an_expiration_plan_rejects_a_compact_job(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let compact = job(&mut conn, &tenant).await;
    let content = serde_json::json!({ "baseline": { "snapshot_id": "41" } });

    let error = expirations::create_expiration_plan(
        &mut conn,
        &NewExpirationPlan {
            id: PlanId::generate(),
            job_id: compact,
            content: &content,
            content_hash: &digest('e'),
        },
        0,
        0,
    )
    .await
    .unwrap_err();

    assert!(
        matches!(error, PersistenceError::InvalidValue { .. }),
        "{error:?}"
    );
    assert!(
        expirations::expiration_plan(&mut conn, compact)
            .await
            .unwrap()
            .is_none()
    );
    let stored = jobs::job_by_id(&mut conn, compact).await.unwrap();
    assert_eq!(stored.state, JobState::Planning);
    assert_eq!(stored.state_revision, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_expiration_intent_rechecks_pins_inside_its_transaction(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let epoch = acquire_leadership(&mut conn, "test", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("leadership is acquired")
        .epoch;
    let expiring = new_job(&mut conn, &tenant, JobOperation::ExpireSnapshots).await;
    let pinned = job(&mut conn, &tenant).await;
    // The pin exists when the expiration plan is written; preparation must
    // recheck it under the table lock.
    plan_at(&mut conn, pinned, 77).await;
    let content = serde_json::json!({ "baseline": { "snapshot_id": "99" }, "remove": [77] });
    let hash = digest('c');
    expirations::create_expiration_plan(
        &mut conn,
        &NewExpirationPlan {
            id: PlanId::generate(),
            job_id: expiring,
            content: &content,
            content_hash: &hash,
        },
        0,
        epoch,
    )
    .await
    .unwrap();

    let intent = bergsmith_persistence::publications::Intent {
        job_id: expiring,
        publication_id: bergsmith_common::ids::PublicationId::generate(),
        content: serde_json::json!({
            "namespace": ["db"],
            "table_name": "t",
            "baseline_snapshot_id": 99,
            "remove": [77],
            "protected": [],
        }),
        request_body: vec![],
        request_hash: digest('d'),
    };
    let error = bergsmith_persistence::publications::create_expiration_intent(
        &mut conn,
        &intent,
        1,
        epoch,
        &[77],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, PersistenceError::SnapshotPinned { snapshot_id: 77 }),
        "{error:?}"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_expiration_intent_waits_for_the_planning_lock(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let epoch = acquire_leadership(&mut conn, "test", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("leadership is acquired")
        .epoch;
    let expiring = new_job(&mut conn, &tenant, JobOperation::ExpireSnapshots).await;
    let pinned = job(&mut conn, &tenant).await;
    let content = serde_json::json!({ "baseline": { "snapshot_id": "99" }, "remove": [77] });
    expirations::create_expiration_plan(
        &mut conn,
        &NewExpirationPlan {
            id: PlanId::generate(),
            job_id: expiring,
            content: &content,
            content_hash: &digest('e'),
        },
        0,
        epoch,
    )
    .await
    .unwrap();
    drop(conn);

    // The planner's transaction holds the table lock and has not committed a
    // plan yet.
    let mut blocker_conn = pool.acquire().await.unwrap();
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker_conn)
        .await
        .unwrap();
    let mut blocker = blocker_conn.begin().await.unwrap();
    plans::lock_table_planning(&mut blocker, tenant.table)
        .await
        .unwrap();

    let intent = bergsmith_persistence::publications::Intent {
        job_id: expiring,
        publication_id: bergsmith_common::ids::PublicationId::generate(),
        content: serde_json::json!({
            "namespace": ["db"],
            "table_name": "t",
            "baseline_snapshot_id": 99,
            "remove": [77],
            "protected": [],
        }),
        request_body: vec![],
        request_hash: digest('f'),
    };
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
    let waiter = pool.clone();
    let task = tokio::spawn(async move {
        let mut conn = waiter.acquire().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        pid_tx.send(pid).unwrap();
        bergsmith_persistence::publications::create_expiration_intent(
            &mut conn,
            &intent,
            1,
            epoch,
            &[77],
        )
        .await
    });
    let waiter_pid = pid_rx.await.unwrap();
    let mut observer = pool.acquire().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        wait_until_blocked(&mut observer, waiter_pid, blocker_pid),
    )
    .await
    .expect("the expiration intent waits on the planning lock");

    sqlx::query(
        "INSERT INTO plans (id, job_id, content, content_hash)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(PlanId::generate().as_uuid())
    .bind(pinned.as_uuid())
    .bind(serde_json::json!({ "baseline": { "snapshot_id": "77" } }))
    .bind(digest('a'))
    .execute(&mut *blocker)
    .await
    .unwrap();
    blocker.commit().await.unwrap();

    let error = task.await.unwrap().unwrap_err();
    assert!(
        matches!(error, PersistenceError::SnapshotPinned { snapshot_id: 77 }),
        "{error:?}"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_plan_cannot_pin_a_snapshot_a_pending_expiration_removes(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let epoch = acquire_leadership(&mut conn, "test", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("leadership is acquired")
        .epoch;
    let expiring = new_job(&mut conn, &tenant, JobOperation::ExpireSnapshots).await;
    let delayed = job(&mut conn, &tenant).await;
    let content = serde_json::json!({
        "baseline": { "snapshot_id": "99" },
        "remove": [99],
    });
    expirations::create_expiration_plan(
        &mut conn,
        &NewExpirationPlan {
            id: PlanId::generate(),
            job_id: expiring,
            content: &content,
            content_hash: &digest('9'),
        },
        0,
        epoch,
    )
    .await
    .unwrap();
    let intent = bergsmith_persistence::publications::Intent {
        job_id: expiring,
        publication_id: bergsmith_common::ids::PublicationId::generate(),
        content: serde_json::json!({
            "namespace": ["db"],
            "table_name": "t",
            "baseline_snapshot_id": 99,
            "remove": [77],
            "protected": [],
        }),
        request_body: vec![],
        request_hash: digest('8'),
    };
    bergsmith_persistence::publications::create_expiration_intent(
        &mut conn,
        &intent,
        1,
        epoch,
        &[77],
    )
    .await
    .unwrap();

    let error = plans::create_plan(
        &mut conn,
        &NewPlan {
            id: PlanId::generate(),
            job_id: delayed,
            content: serde_json::json!({
                // Production planning serializes the i64 as a JSON number.
                "baseline": { "snapshot_id": 77 },
                "groups": 0,
            }),
            content_hash: digest('7'),
            task_ids: Vec::new(),
            execution_budget: Duration::from_secs(180),
        },
    )
    .await
    .expect_err("a delayed plan cannot pin an expiring snapshot");
    assert!(
        matches!(
            error,
            PersistenceError::SnapshotExpiring { snapshot_id: 77 }
        ),
        "{error:?}"
    );
}
