//! Metadata-only rewrite plans: manifest rewrite and dangling-delete removal
//! (BS-031.T3).
#![expect(clippy::expect_used, reason = "test fixtures fail on setup errors")]

mod support;

use std::time::Duration;

use bergsmith_common::ids::{JobId, PlanId};
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::expirations;
use bergsmith_persistence::jobs::{self, NewJob};
use bergsmith_persistence::leadership::acquire_leadership;
use bergsmith_persistence::metadata_plans::{self, NewMetadataPlan};
use sqlx::{PgConnection, PgPool};
use support::{Tenant, digest, force_job_state, job, tenant};

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

async fn epoch(conn: &mut PgConnection) -> i64 {
    acquire_leadership(conn, "test", Duration::from_secs(60))
        .await
        .expect("leadership query succeeds")
        .expect("leadership is acquired")
        .epoch
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_metadata_plan_makes_its_job_ready_and_pins_its_baseline(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let epoch = epoch(&mut conn).await;
    for (operation, snapshot) in [
        (JobOperation::RewriteManifests, 51),
        (JobOperation::RemoveDanglingDeletes, 52),
    ] {
        let planned = new_job(&mut conn, &tenant, operation).await;
        let other = job(&mut conn, &tenant).await;
        let content = serde_json::json!({ "baseline": { "snapshot_id": snapshot.to_string() } });
        let hash = digest('c');

        let ready = metadata_plans::create_metadata_plan(
            &mut conn,
            &NewMetadataPlan {
                id: PlanId::generate(),
                job_id: planned,
                content: &content,
                content_hash: &hash,
            },
            0,
            epoch,
        )
        .await
        .unwrap();

        assert_eq!(ready.state, JobState::ReadyToCommit);
        assert_eq!(ready.operation, operation);
        assert_eq!(
            metadata_plans::metadata_plan(&mut conn, planned)
                .await
                .unwrap(),
            Some((content, hash))
        );
        let protected =
            expirations::protected_snapshots(&mut conn, tenant.organization, tenant.table, other)
                .await
                .unwrap();
        assert!(protected.contains(&snapshot), "{protected:?}");

        force_job_state(&mut conn, planned, JobState::Failed).await;
        let protected =
            expirations::protected_snapshots(&mut conn, tenant.organization, tenant.table, other)
                .await
                .unwrap();
        assert!(!protected.contains(&snapshot), "{protected:?}");

        let update = sqlx::query("UPDATE metadata_plans SET content_hash = $2 WHERE job_id = $1")
            .bind(planned.as_uuid())
            .bind(digest('d'))
            .execute(&mut *conn)
            .await;
        assert!(update.is_err(), "metadata plans are immutable");
        force_job_state(&mut conn, other, JobState::Failed).await;
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_metadata_plan_requires_a_planning_metadata_rewrite_job(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let epoch = epoch(&mut conn).await;
    let compact = job(&mut conn, &tenant).await;
    let content = serde_json::json!({ "baseline": { "snapshot_id": "1" } });
    let plan = |job_id| NewMetadataPlan {
        id: PlanId::generate(),
        job_id,
        content: &content,
        content_hash: "0000000000000000000000000000000000000000000000000000000000000000",
    };

    let error = metadata_plans::create_metadata_plan(&mut conn, &plan(compact), 0, epoch)
        .await
        .unwrap_err();
    assert!(
        matches!(error, PersistenceError::InvalidValue { .. }),
        "{error:?}"
    );

    let ended = new_job(&mut conn, &tenant, JobOperation::RewriteManifests).await;
    force_job_state(&mut conn, ended, JobState::Failed).await;
    let error = metadata_plans::create_metadata_plan(&mut conn, &plan(ended), 0, epoch)
        .await
        .unwrap_err();
    assert!(
        matches!(error, PersistenceError::JobNotInState { .. }),
        "{error:?}"
    );

    let stale = new_job(&mut conn, &tenant, JobOperation::RemoveDanglingDeletes).await;
    let error = metadata_plans::create_metadata_plan(&mut conn, &plan(stale), 0, epoch + 1)
        .await
        .unwrap_err();
    assert!(
        !matches!(error, PersistenceError::InvalidValue { .. }),
        "a stale epoch is a leadership error: {error:?}"
    );
    assert!(
        metadata_plans::metadata_plan(&mut conn, stale)
            .await
            .unwrap()
            .is_none(),
        "nothing commits without leadership"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn only_an_unfinished_compaction_is_in_progress(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let other = support::tenant(&mut conn, "globex").await;
    assert!(
        !metadata_plans::compaction_in_progress(&mut conn, tenant.organization, tenant.table)
            .await
            .unwrap()
    );

    new_job(&mut conn, &tenant, JobOperation::ExpireSnapshots).await;
    new_job(&mut conn, &tenant, JobOperation::RewriteManifests).await;
    assert!(
        !metadata_plans::compaction_in_progress(&mut conn, tenant.organization, tenant.table)
            .await
            .unwrap()
    );

    let compact = job(&mut conn, &tenant).await;
    for state in [
        JobState::Planning,
        JobState::Running,
        JobState::Committing,
        JobState::CommitOutcomeUnknown,
    ] {
        force_job_state(&mut conn, compact, state).await;
        assert!(
            metadata_plans::compaction_in_progress(&mut conn, tenant.organization, tenant.table)
                .await
                .unwrap(),
            "{state}"
        );
    }
    assert!(
        !metadata_plans::compaction_in_progress(&mut conn, other.organization, other.table)
            .await
            .unwrap(),
        "another table is unaffected"
    );
    force_job_state(&mut conn, compact, JobState::Succeeded).await;
    assert!(
        !metadata_plans::compaction_in_progress(&mut conn, tenant.organization, tenant.table)
            .await
            .unwrap()
    );
}
