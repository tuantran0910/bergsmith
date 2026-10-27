//! Immutable orphan-cleanup plans and deletion receipts (BS-031.T2).
#![expect(clippy::unwrap_used, reason = "test fixtures fail on setup errors")]

mod support;

use std::time::Duration;

use bergsmith_common::ids::{JobId, PlanId};
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::jobs::{self, NewJob};
use bergsmith_persistence::leadership::acquire_leadership;
use bergsmith_persistence::orphans::{self, Disposition, NewOrphanPlan, Receipt};
use sqlx::{PgConnection, PgPool};
use support::{Tenant, digest, force_job_state, job, tenant};
use uuid::Uuid;

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
    .unwrap();
    id
}

async fn planned(
    conn: &mut PgConnection,
    tenant: &Tenant,
    epoch: i64,
) -> (JobId, serde_json::Value, String) {
    let id = new_job(conn, tenant, JobOperation::RemoveOrphanFiles).await;
    let content = serde_json::json!({
        "version": 1,
        "job_id": id,
        "candidates": [
            { "path": "s3://b/t/data/a.parquet", "size_bytes": 10, "last_modified_ms": 1 },
            { "path": "s3://b/t/data/b.parquet", "size_bytes": 20, "last_modified_ms": 2 }
        ],
    });
    let hash = digest('a');
    let ready = orphans::create_orphan_plan(
        conn,
        &NewOrphanPlan {
            id: PlanId::generate(),
            job_id: id,
            content: &content,
            content_hash: &hash,
        },
        0,
        epoch,
    )
    .await
    .unwrap();
    assert_eq!(ready.state, JobState::ReadyToCommit);
    assert_eq!(ready.state_revision, 1);
    assert_eq!(ready.operation, JobOperation::RemoveOrphanFiles);
    (id, content, hash)
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_plan_and_its_receipts_carry_the_job_through_deletion(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let epoch = acquire_leadership(&mut conn, "test", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("leadership is acquired")
        .epoch;
    let (id, content, hash) = planned(&mut conn, &tenant, epoch).await;
    assert_eq!(
        orphans::orphan_plan(&mut conn, id).await.unwrap(),
        Some((content, hash))
    );

    let deleting = orphans::begin_deletion(&mut conn, id, 1, epoch)
        .await
        .unwrap();
    assert_eq!(deleting.state, JobState::Deleting);
    assert_eq!(deleting.state_revision, 2);
    assert_eq!(deleting.state_reason.as_deref(), Some("DeletionAuthorized"));

    let receipts = [
        Receipt {
            path: "s3://b/t/data/a.parquet",
            disposition: Disposition::Deleted,
            size_bytes: 10,
        },
        Receipt {
            path: "s3://b/t/data/b.parquet",
            disposition: Disposition::Skipped,
            size_bytes: 20,
        },
    ];
    orphans::record_receipts(&mut conn, id, &receipts, epoch)
        .await
        .unwrap();
    // A crash between the delete and the receipt repeats the same rows.
    orphans::record_receipts(&mut conn, id, &receipts, epoch)
        .await
        .unwrap();
    assert_eq!(
        orphans::recorded_paths(&mut conn, id)
            .await
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>(),
        [
            "s3://b/t/data/a.parquet".to_owned(),
            "s3://b/t/data/b.parquet".to_owned()
        ]
    );
    assert_eq!(
        orphans::receipt_totals(&mut conn, id).await.unwrap(),
        (2, 10)
    );

    let finished = orphans::end_deletion(
        &mut conn,
        id,
        2,
        epoch,
        JobState::Succeeded,
        "OrphanFilesRemoved",
    )
    .await
    .unwrap();
    assert_eq!(finished.state, JobState::Succeeded);
    assert_eq!(finished.state_revision, 3);
    assert_eq!(finished.state_reason.as_deref(), Some("OrphanFilesRemoved"));
    assert!(matches!(
        orphans::begin_deletion(&mut conn, id, 3, epoch)
            .await
            .unwrap_err(),
        PersistenceError::JobNotInState { .. }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_orphan_plan_requires_the_orphan_operation(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let epoch = acquire_leadership(&mut conn, "test", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("leadership is acquired")
        .epoch;
    let compact = job(&mut conn, &tenant).await;
    let content = serde_json::json!({ "candidates": [] });
    let error = orphans::create_orphan_plan(
        &mut conn,
        &NewOrphanPlan {
            id: PlanId::generate(),
            job_id: compact,
            content: &content,
            content_hash: &digest('b'),
        },
        0,
        epoch,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PersistenceError::InvalidValue { .. }));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn deletion_only_begins_from_ready_to_commit(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let epoch = acquire_leadership(&mut conn, "test", Duration::from_secs(60))
        .await
        .unwrap()
        .expect("leadership is acquired")
        .epoch;
    let id = new_job(&mut conn, &tenant, JobOperation::RemoveOrphanFiles).await;
    let error = orphans::begin_deletion(&mut conn, id, 0, epoch)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::JobNotInState {
            state: JobState::Planning,
            ..
        }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn unresolved_publication_manifests_are_reported(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let pending = job(&mut conn, &tenant).await;
    let landed = job(&mut conn, &tenant).await;
    let path = "s3://b/t/metadata/proposal.avro";
    for (id, state, content) in [
        (
            pending,
            JobState::Committing,
            serde_json::json!({"manifest_list": path}),
        ),
        (
            landed,
            JobState::Succeeded,
            serde_json::json!({"manifest_list": "s3://b/t/metadata/old.avro"}),
        ),
        // An expiration intent has no manifest list and contributes nothing.
        (
            job(&mut conn, &tenant).await,
            JobState::Committing,
            serde_json::json!({"baseline_snapshot_id": "1"}),
        ),
    ] {
        force_job_state(&mut conn, id, state).await;
        let publication = Uuid::now_v7();
        sqlx::query("INSERT INTO publications (id, job_id) VALUES ($1, $2)")
            .bind(publication)
            .bind(id.as_uuid())
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO publication_intents
                 (job_id, publication_id, content, request_body, request_hash)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.as_uuid())
        .bind(publication)
        .bind(&content)
        .bind(Vec::<u8>::new())
        .bind(digest('c'))
        .execute(&mut *conn)
        .await
        .unwrap();
    }

    let lists = orphans::unresolved_manifest_lists(
        &mut conn,
        tenant.organization,
        tenant.table,
        JobId::generate(),
    )
    .await
    .unwrap();
    assert_eq!(lists, vec![path.to_owned()]);
}
