//! Per-job event sequences that back reconnectable event streams (BS-018.T3).
#![expect(clippy::unwrap_used, reason = "fixtures fail on setup errors")]

mod support;

use std::borrow::Cow;

use bergsmith_common::ids::JobId;
use bergsmith_common::job::JobState;
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::cancellation::{self, CancelJob};
use bergsmith_persistence::jobs::{self, JobEventKind, Transition};
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use support::{job, sqlstate, tenant};
use uuid::Uuid;

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

async fn sequences(pool: &PgPool, id: JobId) -> Vec<i64> {
    let mut conn = pool.acquire().await.unwrap();
    jobs::job_events(&mut conn, id)
        .await
        .unwrap()
        .iter()
        .map(|event| event.sequence)
        .collect()
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn sequences_start_at_one_and_are_consecutive_per_job(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let first = job(&mut conn, &tenant).await;
    let second = job(&mut conn, &tenant).await;
    // Interleave the two jobs so their global identities interleave too.
    jobs::transition_job(&mut conn, &to(first, 0, JobState::Queued))
        .await
        .unwrap();
    jobs::transition_job(&mut conn, &to(second, 0, JobState::Queued))
        .await
        .unwrap();
    jobs::transition_job(&mut conn, &to(first, 1, JobState::Running))
        .await
        .unwrap();
    drop(conn);

    assert_eq!(sequences(&pool, first).await, [1, 2, 3]);
    assert_eq!(sequences(&pool, second).await, [1, 2]);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn concurrent_writers_on_one_job_leave_no_gaps(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    drop(conn);

    let mut writers = Vec::new();
    for writer in 0..8 {
        let pool = pool.clone();
        writers.push(tokio::spawn(async move {
            let mut conn = pool.acquire().await.unwrap();
            if writer == 0 {
                // Losing the revision race is fine; only the ledger matters.
                let _ = jobs::transition_job(&mut conn, &to(id, 0, JobState::Queued)).await;
            } else {
                cancellation::request_cancellation(
                    &mut conn,
                    &CancelJob {
                        job_id: id,
                        requested_by: None,
                        reason: Some(format!("writer-{writer}")),
                    },
                )
                .await
                .unwrap();
            }
        }));
    }
    for writer in writers {
        writer.await.unwrap();
    }

    let recorded = sequences(&pool, id).await;
    let expected: Vec<i64> = (1..=i64::try_from(recorded.len()).unwrap()).collect();
    assert_eq!(recorded, expected);
    assert!(recorded.len() >= 2, "creation and at least one change");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn events_after_a_sequence_are_paged_in_order(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;
    for (revision, state) in (0..).zip([JobState::Queued, JobState::Running]) {
        jobs::transition_job(&mut conn, &to(id, revision, state))
            .await
            .unwrap();
    }

    let page = jobs::job_events_after(&mut conn, id, 0, 2).await.unwrap();
    assert_eq!(
        page.iter().map(|event| event.sequence).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(page[0].kind, JobEventKind::Created);
    let rest = jobs::job_events_after(&mut conn, id, 2, 2).await.unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].sequence, 3);
    assert_eq!(rest[0].to_state, JobState::Running);
    assert!(
        jobs::job_events_after(&mut conn, id, 3, 2)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        jobs::job_events_after(&mut conn, JobId::generate(), 0, 2)
            .await
            .unwrap()
            .is_empty()
    );
    for (after, limit) in [(-1, 1), (0, 0)] {
        assert!(matches!(
            jobs::job_events_after(&mut conn, id, after, limit).await,
            Err(PersistenceError::InvalidValue { .. })
        ));
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_duplicate_sequence_is_rejected(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let id = job(&mut conn, &tenant).await;

    // An append path that skipped the job lock would compute an existing
    // sequence; the constraint rejects it instead of reordering the stream.
    let error = sqlx::query(
        "INSERT INTO job_events (job_id, sequence, kind, to_state, state_revision)
         VALUES ($1, 1, 'CancellationRequested', 'Planning', 0)",
    )
    .bind(id.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&error), "23505");
}

#[sqlx::test(migrations = false)]
async fn the_migration_numbers_existing_events_per_job_in_append_order(pool: PgPool) {
    let before: Vec<_> = bergsmith_persistence::MIGRATOR
        .iter()
        .filter(|migration| migration.version < 13)
        .cloned()
        .collect();
    let earlier = Migrator {
        migrations: Cow::Owned(before),
        ..Migrator::DEFAULT
    };
    earlier.run(&pool).await.unwrap();

    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let (first, second) = (Uuid::now_v7(), Uuid::now_v7());
    for id in [first, second] {
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, table_identity_id, state, created_by)
             VALUES ($1, $2, $3, 'Planning', $4)",
        )
        .bind(id)
        .bind(tenant.organization.as_uuid())
        .bind(tenant.table.as_uuid())
        .bind(tenant.user.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    }
    for id in [first, second, first, second, first] {
        sqlx::query(
            "INSERT INTO job_events (job_id, kind, to_state, state_revision)
             VALUES ($1, 'CancellationRequested', 'Planning', 0)",
        )
        .bind(id)
        .execute(&mut *conn)
        .await
        .unwrap();
    }
    drop(conn);

    bergsmith_persistence::MIGRATOR.run(&pool).await.unwrap();

    assert_eq!(sequences(&pool, JobId::from_uuid(first)).await, [1, 2, 3]);
    assert_eq!(sequences(&pool, JobId::from_uuid(second)).await, [1, 2]);
    // The immutability trigger is enabled again after the backfill.
    let error = sqlx::query("UPDATE job_events SET reason = 'edited'")
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&error), "23001");
}
