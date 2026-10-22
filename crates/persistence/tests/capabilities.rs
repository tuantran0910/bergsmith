//! Recorded capability checks (BS-022.T2).

use bergsmith_common::capabilities::{CapabilityState, ConnectionCapabilities};
use bergsmith_persistence::capabilities;
use sqlx::PgPool;

mod support;

fn complete() -> ConnectionCapabilities {
    ConnectionCapabilities {
        list_tables: CapabilityState::Met,
        load_table: CapabilityState::Met,
        commit_table: CapabilityState::Met,
        vended_credentials: CapabilityState::Unknown,
        credentials_endpoint: CapabilityState::Met,
        // A lifetime beyond what a browser counts exactly survives the
        // ledger unchanged.
        idempotency_key_lifetime_seconds: Some(9_007_199_254_740_993),
    }
}

fn incomplete() -> ConnectionCapabilities {
    ConnectionCapabilities {
        list_tables: CapabilityState::Met,
        load_table: CapabilityState::Met,
        commit_table: CapabilityState::Missing,
        vended_credentials: CapabilityState::Unknown,
        credentials_endpoint: CapabilityState::Missing,
        idempotency_key_lifetime_seconds: Some(1800),
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_check_is_recorded_replaced_and_scoped_to_its_organization(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other = support::tenant(&mut conn, "other").await;

    assert!(
        capabilities::latest_check(&mut conn, tenant.organization, tenant.connection)
            .await
            .unwrap()
            .is_none()
    );

    let recorded = complete();
    let checked_at = capabilities::record_check(
        &mut conn,
        tenant.organization,
        tenant.connection,
        &recorded,
        "fingerprint-a",
    )
    .await
    .unwrap();
    let check = capabilities::latest_check(&mut conn, tenant.organization, tenant.connection)
        .await
        .unwrap()
        .expect("the check is recorded");
    assert_eq!(check.capabilities, recorded);
    assert_eq!(
        check.capabilities.idempotency_key_lifetime_seconds,
        Some(9_007_199_254_740_993)
    );
    assert_eq!(check.config_fingerprint, "fingerprint-a");
    assert_eq!(check.checked_at, checked_at);
    assert!(recorded.complete());

    // A later check replaces the earlier answer for the same connection.
    let replaced = incomplete();
    let later = capabilities::record_check(
        &mut conn,
        tenant.organization,
        tenant.connection,
        &replaced,
        "fingerprint-b",
    )
    .await
    .unwrap();
    let check = capabilities::latest_check(&mut conn, tenant.organization, tenant.connection)
        .await
        .unwrap()
        .expect("the check is recorded");
    assert_eq!(check.capabilities, replaced);
    assert_eq!(check.config_fingerprint, "fingerprint-b");
    assert!(later >= checked_at);
    assert!(!replaced.complete());

    // Another organization's connection has no check, and this connection's
    // check is not visible from another organization.
    assert!(
        capabilities::latest_check(&mut conn, other.organization, other.connection)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        capabilities::latest_check(&mut conn, other.organization, tenant.connection)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_check_of_an_unknown_connection_is_refused(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let unknown = bergsmith_common::ids::CatalogConnectionId::generate();

    let error = capabilities::record_check(
        &mut conn,
        tenant.organization,
        unknown,
        &complete(),
        "fingerprint",
    )
    .await
    .unwrap_err();

    assert!(
        matches!(
            error,
            bergsmith_persistence::PersistenceError::MissingReference { .. }
        ),
        "{error:?}"
    );
}
