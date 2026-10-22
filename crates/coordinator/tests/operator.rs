//! Operator role commands: grant, revoke, and list (BS-020.T7).
#![expect(clippy::unwrap_used, reason = "fixtures fail on setup errors")]

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_coordinator::operator::{self, OperatorError, RoleAssignment};
use bergsmith_persistence::{PersistenceError, grants, tenancy};
use sqlx::PgPool;

fn assignment(tenant: &support::Tenant, subject: &str, role: Role) -> RoleAssignment {
    RoleAssignment {
        organization: "acme".into(),
        actor: tenant.user,
        issuer: "https://idp.example".into(),
        subject: subject.into(),
        display_name: subject.into(),
        role,
        scope: GrantScope::table(tenant.table),
    }
}

async fn audited(pool: &PgPool, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action = $1")
        .bind(action)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn granting_provisions_a_new_person_once_and_audits_each_change(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;

    let viewer = operator::grant_role(&mut conn, &assignment(&tenant, "carol", Role::Viewer))
        .await
        .unwrap();
    let operator_grant =
        operator::grant_role(&mut conn, &assignment(&tenant, "carol", Role::Operator))
            .await
            .unwrap();

    let principal = tenancy::principal_by_subject(
        &mut conn,
        tenant.organization,
        "https://idp.example",
        "carol",
    )
    .await
    .unwrap();
    assert_eq!(viewer.principal_id, principal.id);
    assert_eq!(operator_grant.principal_id, principal.id);
    assert_eq!(viewer.scope, GrantScope::table(tenant.table));
    assert_eq!(viewer.granted_by, Some(tenant.user));
    assert_eq!(audited(&pool, "identity.provisioned").await, 1);
    assert_eq!(audited(&pool, "grant.created").await, 2);
    let error = operator::grant_role(&mut conn, &assignment(&tenant, "carol", Role::Viewer))
        .await
        .unwrap_err();
    assert!(
        matches!(error, OperatorError::Administration(_)),
        "a duplicate grant is refused: {error}"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn only_an_administrator_may_be_recorded_as_the_actor(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let viewer = operator::grant_role(&mut conn, &assignment(&tenant, "dave", Role::Viewer))
        .await
        .unwrap();

    let mut escalation = assignment(&tenant, "dave", Role::Administrator);
    escalation.actor = viewer.principal_id;
    escalation.scope = GrantScope::organization();
    let error = operator::grant_role(&mut conn, &escalation)
        .await
        .unwrap_err();
    assert!(matches!(error, OperatorError::Administration(_)), "{error}");
    let error = operator::revoke_role(&mut conn, "acme", viewer.principal_id, viewer.id)
        .await
        .unwrap_err();
    assert!(matches!(error, OperatorError::Administration(_)), "{error}");

    let error = operator::grant_role(
        &mut conn,
        &assignment(&tenant, "svc", Role::MaintenanceService),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, OperatorError::ServiceRole(_)), "{error}");
    let error = operator::list_grants(&mut conn, "acme", viewer.principal_id)
        .await
        .unwrap_err();
    assert!(matches!(error, OperatorError::Authorization(_)), "{error}");
    let mut unknown = assignment(&tenant, "erin", Role::Viewer);
    unknown.organization = "missing".into();
    let error = operator::grant_role(&mut conn, &unknown).await.unwrap_err();
    assert!(
        matches!(
            error,
            OperatorError::Persistence(PersistenceError::NotFound { .. })
        ),
        "{error}"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn revoked_grants_leave_the_listing(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other = support::tenant(&mut conn, "other").await;
    let granted = operator::grant_role(&mut conn, &assignment(&tenant, "frank", Role::Operator))
        .await
        .unwrap();

    let listed = operator::list_grants(&mut conn, "acme", tenant.user)
        .await
        .unwrap();
    assert_eq!(listed.len(), 2, "the tenant administrator and frank");
    let frank = listed
        .iter()
        .find(|listing| listing.grant.id == granted.id)
        .unwrap();
    assert_eq!(frank.principal.subject, "frank");
    assert!(
        listed
            .iter()
            .all(|listing| listing.principal.id != other.user)
    );

    let revoked = operator::revoke_role(&mut conn, "acme", tenant.user, granted.id)
        .await
        .unwrap();
    assert_eq!(revoked.revoked_by, Some(tenant.user));
    assert_eq!(audited(&pool, "grant.revoked").await, 1);
    let listed = operator::list_grants(&mut conn, "acme", tenant.user)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert!(
        grants::active_grants(&mut conn, tenant.organization, granted.principal_id)
            .await
            .unwrap()
            .is_empty()
    );
    let error = operator::revoke_role(&mut conn, "acme", tenant.user, granted.id)
        .await
        .unwrap_err();
    assert!(matches!(error, OperatorError::Administration(_)), "{error}");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_service_principal_cannot_receive_a_role_for_people(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let service = support::service_principal(&mut conn, tenant.organization, "svc-1").await;

    for role in [Role::Viewer, Role::Operator, Role::Administrator] {
        let mut request = assignment(&tenant, "svc-1", role);
        request.issuer = "bergsmith:test".into();
        let error = operator::grant_role(&mut conn, &request).await.unwrap_err();
        assert!(
            matches!(error, OperatorError::ServicePrincipal(id) if id == service),
            "{error}"
        );
    }
    assert!(
        grants::active_grants(&mut conn, tenant.organization, service)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(audited(&pool, "grant.created").await, 0);
}
