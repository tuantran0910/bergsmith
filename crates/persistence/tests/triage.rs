//! Job triage reads against the real ledger (BS-032.T5).

#![allow(clippy::unwrap_used, reason = "fixtures fail the test immediately")]

use bergsmith_common::job::JobState;
use bergsmith_common::task::TaskState;
use bergsmith_persistence::triage;
use sqlx::PgPool;

#[path = "support/mod.rs"]
mod support;

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn tasks_and_attempts_carry_failure_duration_and_outputs(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "triage").await;
    let job = support::job(&mut conn, &tenant).await;
    let tasks = support::plan(&mut conn, job, 2).await;
    support::force_job_state(&mut conn, job, JobState::Running).await;
    let failed = support::grant(&mut conn, tasks[0]).await.unwrap();
    let completed = support::grant(&mut conn, tasks[1]).await.unwrap();
    sqlx::query(
        "INSERT INTO attempt_failures (attempt_id, reason_code, disposition, job_state) \
         VALUES ($1, 'ProcessMemoryLimitExceeded', 'PermanentFailure', 'Failed')",
    )
    .bind(failed.id.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE attempts SET state = 'Failed', finished_at = granted_at + interval '4 seconds' \
         WHERE id = $1",
    )
    .bind(failed.id.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("UPDATE tasks SET state = 'Failed' WHERE id = $1")
        .bind(tasks[0].as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    // Fixed ledger fixture: a receipt and its verified inventory are normally
    // committed by the worker result acceptance transaction.
    sqlx::query(
        "INSERT INTO receipts (attempt_id, task_id, content, content_hash) VALUES ($1,$2,'[]',$3)",
    )
    .bind(completed.id.as_uuid())
    .bind(tasks[1].as_uuid())
    .bind(support::digest('b'))
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("INSERT INTO attempt_output_prefixes (attempt_id, prefix) VALUES ($1,$2)")
        .bind(completed.id.as_uuid())
        .bind(format!("s3://fixture/{}/", completed.id))
        .execute(&mut *conn)
        .await
        .unwrap();
    for (path, size) in [
        ("s3://fixture/output-1.parquet", 9_007_199_254_740_993_i64),
        ("s3://fixture/output-2.parquet", 7),
    ] {
        sqlx::query(
            "INSERT INTO attempt_output_objects (attempt_id, path, size_bytes) VALUES ($1,$2,$3)",
        )
        .bind(completed.id.as_uuid())
        .bind(path)
        .bind(size)
        .execute(&mut *conn)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE attempts SET state = 'Accepted', finished_at = granted_at + interval '9 seconds' WHERE id = $1")
        .bind(completed.id.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();

    let first = triage::tasks_after(&mut conn, job, None, 2).await.unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].ordinal, 0);
    assert_eq!(first[0].state, TaskState::Failed);
    assert_eq!(first[1].state, TaskState::Leased);
    assert_eq!(first[0].attempts_started, 1);
    assert!(first[0].retry_not_before.is_none());

    // The keyset is the plan ordinal, not the row identity.
    let second = triage::tasks_after(&mut conn, job, Some(0), 2)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].id, tasks[1]);

    let attempts = triage::attempts_for(&mut conn, &tasks).await.unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[0].task_id, tasks[0]);
    assert_eq!(
        attempts[0].state,
        bergsmith_common::task::AttemptState::Failed
    );
    assert_eq!(
        attempts[0].failure_reason.as_deref(),
        Some("ProcessMemoryLimitExceeded")
    );
    assert_eq!(
        attempts[0].failure_disposition,
        Some(bergsmith_common::disposition::FailureDisposition::PermanentFailure)
    );
    assert_eq!(attempts[0].pool, "test");
    assert_eq!(attempts[0].worker_name, "test");
    assert_eq!(
        attempts[0]
            .finished_at
            .unwrap()
            .signed_duration_since(attempts[0].granted_at),
        chrono::TimeDelta::seconds(4)
    );
    assert_eq!(attempts[1].output_files, 2);
    assert_eq!(attempts[1].output_bytes, 9_007_199_254_741_000);
    assert_eq!(attempts[1].failure_reason, None);

    let empty = triage::attempts_for(&mut conn, &[]).await.unwrap();
    assert!(empty.is_empty());
}
