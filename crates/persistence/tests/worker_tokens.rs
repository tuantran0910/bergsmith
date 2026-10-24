//! Worker pool token storage and authentication (BS-023.T4).

#![expect(
    clippy::unwrap_used,
    reason = "integration fixtures fail the test on setup errors"
)]
mod support;

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{OrganizationId, PrincipalId, WorkerPoolTokenId};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::worker_tokens::{self, NewWorkerPoolToken};
use sqlx::PgConnection;
use sqlx::PgPool;

fn digest(seed: &str) -> Sha256Digest {
    Sha256Digest::of(b"bergsmith.test-token.v1", seed.as_bytes())
}

fn new_token(
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    pool: &str,
    seed: &str,
) -> NewWorkerPoolToken {
    NewWorkerPoolToken {
        id: WorkerPoolTokenId::generate(),
        organization_id,
        principal_id,
        pool: pool.to_owned(),
        label: Some(seed.to_owned()),
        verifier: digest(seed),
        created_by: None,
        rotated_from: None,
    }
}

async fn service_with_token(
    conn: &mut PgConnection,
    pool: &str,
    seed: &str,
) -> (OrganizationId, PrincipalId, WorkerPoolTokenId) {
    let tenant = support::tenant(conn, "acme").await;
    let principal = support::service_principal(conn, tenant.organization, "workers").await;
    let token = worker_tokens::create(conn, &new_token(tenant.organization, principal, pool, seed))
        .await
        .unwrap();
    (tenant.organization, principal, token.id)
}

#[sqlx::test]
async fn a_token_authenticates_until_it_is_revoked(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let (organization, principal, token_id) =
        service_with_token(&mut conn, "native", "before").await;

    let identity = worker_tokens::authenticate(&mut conn, "native", &digest("before"))
        .await
        .unwrap()
        .expect("an active verifier authenticates");
    assert_eq!(identity.organization, organization);
    assert_eq!(identity.principal, principal);
    assert_eq!(identity.pool, "native");

    assert!(
        worker_tokens::authenticate(&mut conn, "other", &digest("before"))
            .await
            .unwrap()
            .is_none(),
        "a token is scoped to its pool"
    );
    assert!(
        worker_tokens::authenticate(&mut conn, "native", &digest("different"))
            .await
            .unwrap()
            .is_none(),
        "a different verifier does not authenticate"
    );

    let revoked = worker_tokens::revoke(
        &mut conn,
        organization,
        principal,
        token_id,
        Some(principal),
    )
    .await
    .unwrap();
    assert!(!revoked.is_active());
    assert!(
        worker_tokens::authenticate(&mut conn, "native", &digest("before"))
            .await
            .unwrap()
            .is_none(),
        "revocation applies immediately"
    );
}

#[sqlx::test]
async fn deactivating_the_service_identity_stops_authentication(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let (organization, principal, _) = service_with_token(&mut conn, "native", "paused").await;

    let (_, changed) =
        bergsmith_persistence::tenancy::deactivate_principal(&mut conn, organization, principal)
            .await
            .unwrap();
    assert!(changed);
    assert!(
        worker_tokens::authenticate(&mut conn, "native", &digest("paused"))
            .await
            .unwrap()
            .is_none(),
        "a deactivated principal's tokens authenticate nothing"
    );

    let (_, restored) =
        bergsmith_persistence::tenancy::reactivate_principal(&mut conn, organization, principal)
            .await
            .unwrap();
    assert!(restored);
    assert!(
        worker_tokens::authenticate(&mut conn, "native", &digest("paused"))
            .await
            .unwrap()
            .is_some(),
        "reactivation restores the token because revocation is separate"
    );
}

#[sqlx::test]
async fn a_duplicate_verifier_is_rejected(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let (organization, principal, _) = service_with_token(&mut conn, "native", "duplicate").await;

    let error = worker_tokens::create(
        &mut conn,
        &new_token(organization, principal, "other", "duplicate"),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            PersistenceError::AlreadyExists {
                entity: "worker pool token",
                ..
            }
        ),
        "{error:?}"
    );
}

#[sqlx::test]
async fn tokens_are_listed_in_identifier_order_after_the_cursor(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let (organization, principal, first) = service_with_token(&mut conn, "native", "first").await;
    let second = worker_tokens::create(
        &mut conn,
        &new_token(organization, principal, "native", "second"),
    )
    .await
    .unwrap();

    let page = worker_tokens::list(&mut conn, organization, principal, None, 2)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|token| token.id).collect::<Vec<_>>(),
        vec![first, second.id]
    );

    let after = worker_tokens::list(&mut conn, organization, principal, Some(first), 2)
        .await
        .unwrap();
    assert_eq!(
        after.iter().map(|token| token.id).collect::<Vec<_>>(),
        vec![second.id]
    );
}

#[sqlx::test]
async fn a_person_principal_cannot_authenticate_a_worker(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    // No API path creates this row for a person; the authentication query
    // still refuses it if a malformed or migrated ledger contains one.
    let token_id = WorkerPoolTokenId::generate();
    let verifier = digest("person");
    sqlx::query(
        "INSERT INTO worker_pool_tokens
             (id, organization_id, principal_id, pool, verifier)
         VALUES ($1, $2, $3, 'native', $4)",
    )
    .bind(token_id.as_uuid())
    .bind(tenant.organization.as_uuid())
    .bind(tenant.user.as_uuid())
    .bind(verifier.as_bytes().as_slice())
    .execute(&mut *conn)
    .await
    .unwrap();

    assert!(
        worker_tokens::authenticate(&mut conn, "native", &digest("person"))
            .await
            .unwrap()
            .is_none()
    );
}
