//! Idempotency keys bound to request hashes within organization scope
//! against real PostgreSQL (BS-003.T7).

mod support;

use bergsmith_common::ids::OrganizationId;
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::idempotency::{self, IdempotencyKey, IdempotencyStatus, RecordOutcome};
use sqlx::{Connection, PgPool};
use support::{constraint, digest, organization, sqlstate};
use uuid::Uuid;

fn key(organization_id: OrganizationId, request: char) -> IdempotencyKey {
    IdempotencyKey {
        organization_id,
        key: "submit-1".to_owned(),
        operation: "SubmitJob".to_owned(),
        request_hash: digest(request),
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unused_key_is_new_and_a_recorded_key_replays(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let resource = Uuid::now_v7();

    let before = idempotency::check(&mut conn, &key(acme, 'a'))
        .await
        .unwrap();
    let recorded = idempotency::record(&mut conn, &key(acme, 'a'), resource)
        .await
        .unwrap();
    let after = idempotency::check(&mut conn, &key(acme, 'a'))
        .await
        .unwrap();

    assert_eq!(before, IdempotencyStatus::New);
    assert_eq!(recorded, RecordOutcome::Recorded);
    assert_eq!(
        after,
        IdempotencyStatus::Replay {
            resource_id: resource
        }
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_key_reused_for_a_different_request_is_rejected(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    idempotency::record(&mut conn, &key(acme, 'a'), Uuid::now_v7())
        .await
        .unwrap();
    let mut other_operation = key(acme, 'a');
    other_operation.operation = "CancelJob".to_owned();

    let other_hash = idempotency::check(&mut conn, &key(acme, 'b'))
        .await
        .unwrap_err();
    let other_op = idempotency::check(&mut conn, &other_operation)
        .await
        .unwrap_err();
    let record_again = idempotency::record(&mut conn, &key(acme, 'b'), Uuid::now_v7())
        .await
        .unwrap_err();

    for error in [other_hash, other_op, record_again] {
        assert!(matches!(
            error,
            PersistenceError::IdempotencyKeyReused { .. }
        ));
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn keys_are_scoped_to_their_organization(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let globex = organization(&mut conn, "globex").await;
    idempotency::record(&mut conn, &key(acme, 'a'), Uuid::now_v7())
        .await
        .unwrap();

    let status = idempotency::check(&mut conn, &key(globex, 'b'))
        .await
        .unwrap();
    let recorded = idempotency::record(&mut conn, &key(globex, 'b'), Uuid::now_v7())
        .await
        .unwrap();

    assert_eq!(status, IdempotencyStatus::New);
    assert_eq!(recorded, RecordOutcome::Recorded);
}

/// Records the key in its own transaction, committing only if it won.
async fn record_in_transaction(
    conn: &mut sqlx::PgConnection,
    organization_id: OrganizationId,
    resource: Uuid,
) -> Result<RecordOutcome, PersistenceError> {
    let mut tx = conn.begin().await?;
    let outcome = idempotency::record(&mut tx, &key(organization_id, 'a'), resource).await?;
    if outcome == RecordOutcome::Recorded {
        tx.commit().await?;
    }
    Ok(outcome)
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn concurrent_identical_requests_resolve_to_the_first_resource(pool: PgPool) {
    let mut setup = pool.acquire().await.unwrap();
    let acme = organization(&mut setup, "acme").await;
    drop(setup);
    let mut first = pool.acquire().await.unwrap();
    let mut second = pool.acquire().await.unwrap();
    let (a_resource, b_resource) = (Uuid::now_v7(), Uuid::now_v7());

    let (a, b) = tokio::join!(
        record_in_transaction(&mut first, acme, a_resource),
        record_in_transaction(&mut second, acme, b_resource),
    );

    let (a, b) = (a.unwrap(), b.unwrap());
    let mut outcomes = [a, b];
    outcomes.sort_by_key(|o| matches!(o, RecordOutcome::Replay { .. }));
    assert_eq!(outcomes[0], RecordOutcome::Recorded);
    let RecordOutcome::Replay { resource_id } = outcomes[1] else {
        panic!("expected one replay, got {outcomes:?}");
    };
    let winner = if a == RecordOutcome::Recorded {
        a_resource
    } else {
        b_resource
    };
    assert_eq!(resource_id, winner);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn keys_and_hashes_are_validated(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    let mut empty = key(acme, 'a');
    empty.key = String::new();
    let mut bad_hash = key(acme, 'a');
    bad_hash.request_hash = "nope".to_owned();

    let empty = idempotency::record(&mut conn, &empty, Uuid::now_v7())
        .await
        .unwrap_err();
    let bad_hash = idempotency::record(&mut conn, &bad_hash, Uuid::now_v7())
        .await
        .unwrap_err();

    assert_eq!(constraint(&empty), "idempotency_keys_key_check");
    assert_eq!(constraint(&bad_hash), "idempotency_keys_request_hash_check");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn recorded_keys_are_immutable(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = organization(&mut conn, "acme").await;
    idempotency::record(&mut conn, &key(acme, 'a'), Uuid::now_v7())
        .await
        .unwrap();

    for statement in [
        "UPDATE idempotency_keys SET resource_id = gen_random_uuid()",
        "DELETE FROM idempotency_keys",
    ] {
        let error = sqlx::query(statement).execute(&pool).await.unwrap_err();
        assert_eq!(sqlstate(&error), "23001", "{statement}");
    }
}
