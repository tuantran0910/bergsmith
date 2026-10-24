//! OIDC sessions, scoped authorization, maintenance service identity, and
//! audit against real PostgreSQL (BS-017).
#![expect(
    clippy::unwrap_used,
    reason = "integration fixtures fail on setup errors"
)]

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

use bergsmith_common::audit::{ActorKind, AuditAction, AuditOutcome};
use bergsmith_common::ids::{JobId, PrincipalId, RoleGrantId};
use bergsmith_common::permission::{GrantScope, Permission, ResourceScope, Role};
use bergsmith_coordinator::identity::{
    administration, audit,
    authorization::{Authorizer, MaintenanceService},
    session::{self, SessionConfig},
};
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::scheduling::AdmissionLimits;
use bergsmith_persistence::tenancy::{self, NewPrincipal, PrincipalKind};
use bergsmith_persistence::{PersistenceError, audit as audit_ledger, jobs};
use serde_json::json;
use sqlx::PgPool;
use support::{Tenant, tenant};

fn grant(
    organization: bergsmith_common::ids::OrganizationId,
    principal: PrincipalId,
    role: Role,
    scope: GrantScope,
) -> NewRoleGrant {
    NewRoleGrant {
        id: RoleGrantId::generate(),
        organization_id: organization,
        principal_id: principal,
        role,
        scope,
        granted_by: None,
    }
}

async fn service_principal(
    conn: &mut sqlx::PgConnection,
    organization: bergsmith_common::ids::OrganizationId,
    subject: &str,
) -> PrincipalId {
    let id = PrincipalId::generate();
    tenancy::create_principal(
        conn,
        &NewPrincipal {
            id,
            organization_id: organization,
            kind: PrincipalKind::Service,
            issuer: "https://idp.example".to_owned(),
            subject: subject.to_owned(),
            display_name: subject.to_owned(),
        },
    )
    .await
    .unwrap();
    id
}

async fn planning_job(
    conn: &mut sqlx::PgConnection,
    fixtures: &Tenant,
    created_by: PrincipalId,
) -> JobId {
    let id = JobId::generate();
    jobs::create_job(
        conn,
        &jobs::NewJob {
            id,
            organization_id: fixtures.organization,
            table_id: fixtures.table,
            retry_of: None,
            created_by,
        },
    )
    .await
    .unwrap();
    id
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn sessions_issue_authenticate_check_csrf_and_log_out(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let config = SessionConfig {
        secure: false,
        ..SessionConfig::default()
    };

    let issued = session::issue(
        &mut conn,
        &config,
        fixtures.organization,
        fixtures.user,
        "https://idp.example",
        "alice",
    )
    .await
    .unwrap();
    let authenticated = session::authenticate(&mut conn, &config, &issued.token)
        .await
        .unwrap();
    assert_eq!(authenticated.principal_id, fixtures.user);
    assert!(session::verify_csrf(&authenticated, issued.csrf.expose()).is_ok());
    assert!(session::verify_csrf(&authenticated, "not-the-token").is_err());

    assert!(session::logout(&mut conn, &issued.token).await.unwrap());
    assert!(
        session::authenticate(&mut conn, &config, &issued.token)
            .await
            .is_err()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn visible_scopes_isolate_connections_and_tables(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let other = tenant(&mut conn, "other").await;
    let scoped = support::user(&mut conn, fixtures.organization, "scoped").await;

    grants::create_grant(
        &mut conn,
        &grant(
            fixtures.organization,
            scoped,
            Role::Viewer,
            GrantScope::connection(fixtures.connection),
        ),
    )
    .await
    .unwrap();

    let connections = Authorizer::visible_connections(
        &mut conn,
        fixtures.organization,
        scoped,
        Permission::ViewInventory,
    )
    .await
    .unwrap();
    assert_eq!(connections, vec![fixtures.connection]);

    let tables = Authorizer::visible_tables(
        &mut conn,
        fixtures.organization,
        scoped,
        Permission::ViewInventory,
    )
    .await
    .unwrap();
    assert_eq!(tables, vec![fixtures.table]);

    // Nothing outside the organization is visible, even for an administrator.
    let cross = Authorizer::visible_tables(
        &mut conn,
        other.organization,
        fixtures.user,
        Permission::ViewInventory,
    )
    .await
    .unwrap();
    assert!(cross.is_empty());

    // A table-scoped viewer sees its table but not its connection.
    let table_scoped = support::user(&mut conn, fixtures.organization, "table-scoped").await;
    grants::create_grant(
        &mut conn,
        &grant(
            fixtures.organization,
            table_scoped,
            Role::Viewer,
            GrantScope::table(fixtures.table),
        ),
    )
    .await
    .unwrap();
    assert!(
        Authorizer::visible_connections(
            &mut conn,
            fixtures.organization,
            table_scoped,
            Permission::ViewInventory,
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        Authorizer::visible_tables(
            &mut conn,
            fixtures.organization,
            table_scoped,
            Permission::ViewInventory,
        )
        .await
        .unwrap(),
        vec![fixtures.table]
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_maintenance_service_binds_only_service_accounts_with_a_grant(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;

    // A person is never a schedule identity.
    assert!(
        MaintenanceService::bind(&mut conn, fixtures.organization, fixtures.user)
            .await
            .is_err()
    );

    let service = service_principal(&mut conn, fixtures.organization, "scheduler").await;
    // A service account without a grant cannot be bound.
    assert!(
        MaintenanceService::bind(&mut conn, fixtures.organization, service)
            .await
            .is_err()
    );

    grants::create_grant(
        &mut conn,
        &grant(
            fixtures.organization,
            service,
            Role::MaintenanceService,
            GrantScope::table(fixtures.table),
        ),
    )
    .await
    .unwrap();
    let bound = MaintenanceService::bind(&mut conn, fixtures.organization, service)
        .await
        .unwrap();
    assert_eq!(bound.principal_id(), service);

    let job = planning_job(&mut conn, &fixtures, service).await;
    let job = jobs::job(&mut conn, fixtures.organization, job)
        .await
        .unwrap();
    bound
        .authorize_job(&mut conn, &job, Permission::SubmitJobs)
        .await
        .unwrap();

    // Revoking the grant stops the service identity; worker rights are a
    // separate token path and are not consulted here.
    grants::revoke_principal_grants(&mut conn, fixtures.organization, service, None)
        .await
        .unwrap();
    let error = bound
        .authorize_job(&mut conn, &job, Permission::SubmitJobs)
        .await
        .unwrap_err();
    assert_eq!(error.reason_code(), "AuthorizationDenied");
    assert!(matches!(
        grants::require_job_permission(&mut conn, &job, Permission::SubmitJobs).await,
        Err(PersistenceError::PermissionDenied { .. })
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn admission_denies_a_requester_whose_grant_was_revoked(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let job = planning_job(&mut conn, &fixtures, fixtures.user).await;
    support::force_job_state(&mut conn, job, bergsmith_common::job::JobState::Queued).await;

    // With the tenant's administrator grant, admission passes authorization and
    // fails later (leadership or missing dispatch configuration); the exact
    // error is not the point, only that authorization allowed the attempt.
    let error =
        bergsmith_persistence::scheduling::admit(&mut conn, job, 1, AdmissionLimits::default())
            .await
            .unwrap_err();
    assert!(
        !matches!(error, PersistenceError::PermissionDenied { .. }),
        "authorization must allow the grant-holder: {error:?}"
    );

    grants::revoke_principal_grants(&mut conn, fixtures.organization, fixtures.user, None)
        .await
        .unwrap();
    let error =
        bergsmith_persistence::scheduling::admit(&mut conn, job, 1, AdmissionLimits::default())
            .await
            .unwrap_err();
    assert!(matches!(error, PersistenceError::PermissionDenied { .. }));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn identity_and_maintenance_decisions_are_auditable(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;

    audit::login_succeeded(
        &mut conn,
        fixtures.organization,
        fixtures.user,
        "https://idp.example",
    )
    .await
    .unwrap();
    audit::login_failed(
        &mut conn,
        Some(fixtures.organization),
        Some("https://idp.example"),
        "identity_not_provisioned",
    )
    .await
    .unwrap();
    audit::logged_out(&mut conn, fixtures.organization, fixtures.user)
        .await
        .unwrap();
    audit::grant_changed(
        &mut conn,
        fixtures.organization,
        Some(fixtures.user),
        AuditAction::GrantCreated,
        fixtures.user,
        "Operator",
        None,
    )
    .await
    .unwrap();
    audit::job_decision(
        &mut conn,
        fixtures.organization,
        Some(fixtures.user),
        AuditAction::JobAdmissionDenied,
        "job-1",
        AuditOutcome::Denied,
        Some("AuthorizationRevoked"),
    )
    .await
    .unwrap();
    record_denied_publication(&mut conn, fixtures.organization, fixtures.user).await;
    let entries = audit_ledger::list(&mut conn, fixtures.organization, None, 100)
        .await
        .unwrap();
    let actions: Vec<&str> = entries.iter().map(|entry| entry.action.as_str()).collect();
    for expected in [
        AuditAction::AuthLoginSucceeded,
        AuditAction::AuthLoginFailed,
        AuditAction::AuthLoggedOut,
        AuditAction::GrantCreated,
        AuditAction::JobAdmissionDenied,
        AuditAction::JobPublicationDenied,
    ] {
        assert!(actions.contains(&expected.as_str()), "missing {expected:?}");
    }
    let denied = entries
        .iter()
        .find(|entry| entry.action == AuditAction::JobAdmissionDenied.as_str())
        .unwrap();
    assert_eq!(denied.actor_principal_id, Some(fixtures.user));
    assert_eq!(denied.actor_kind, ActorKind::User);
    assert_eq!(denied.outcome, AuditOutcome::Denied);
    assert_eq!(denied.detail, json!({ "reason": "AuthorizationRevoked" }));

    let anonymous = entries
        .iter()
        .find(|entry| entry.action == AuditAction::AuthLoginFailed.as_str())
        .unwrap();
    assert_eq!(anonymous.actor_kind, ActorKind::Anonymous);
    assert_eq!(anonymous.actor_principal_id, None);
    assert_eq!(anonymous.actor_display_name, None);
}

async fn record_denied_publication(
    conn: &mut sqlx::PgConnection,
    organization: bergsmith_common::ids::OrganizationId,
    actor: PrincipalId,
) {
    bergsmith_coordinator::identity::routes::audit_denied_command(
        conn,
        organization,
        Some(actor),
        AuditAction::JobPublicationDenied,
        "job",
        Some("job-1"),
        "AuthorizationRevoked",
    )
    .await
    .unwrap();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_job_requester_is_mandatory_and_authorized(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;

    // The schema refuses an unattributed job, so there is no bypass to forget.
    let nullable: String = sqlx::query_scalar(
        "SELECT is_nullable FROM information_schema.columns
         WHERE table_name = 'jobs' AND column_name = 'created_by'",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(nullable, "NO");
    let insert = sqlx::query(
        "INSERT INTO jobs (id, organization_id, table_identity_id, state, created_by)
         VALUES (gen_random_uuid(), $1, $2, 'Planning', NULL)",
    )
    .bind(fixtures.organization.as_uuid())
    .bind(fixtures.table.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap_err();
    assert_eq!(
        insert.as_database_error().unwrap().code().as_deref(),
        Some("23502")
    );

    // A normal requester is authorized through its grant.
    let job = planning_job(&mut conn, &fixtures, fixtures.user).await;
    let job = jobs::job(&mut conn, fixtures.organization, job)
        .await
        .unwrap();
    grants::require_job_permission(&mut conn, &job, Permission::SubmitJobs)
        .await
        .unwrap();
    assert_eq!(job.created_by, fixtures.user);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cross_organization_grants_do_not_authorize(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let other = tenant(&mut conn, "other").await;
    let job = planning_job(&mut conn, &other, other.user).await;
    let job = jobs::job(&mut conn, other.organization, job).await.unwrap();

    // The other organization's principal holds a grant in its own organization
    // only; the acme resource is never covered.
    assert!(matches!(
        grants::require_permission(
            &mut conn,
            fixtures.organization,
            other.user,
            Permission::SubmitJobs,
            ResourceScope::Table(fixtures.table),
        )
        .await,
        Err(PersistenceError::PermissionDenied { .. })
    ));
    assert!(matches!(
        Authorizer::require(
            &mut conn,
            fixtures.organization,
            other.user,
            Permission::SubmitJobs,
            ResourceScope::Table(fixtures.table),
        )
        .await,
        Err(bergsmith_coordinator::identity::authorization::AuthorizationError::Denied { .. })
    ));
    // Its own organization's job is still authorized.
    grants::require_job_permission(&mut conn, &job, Permission::SubmitJobs)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn administration_requires_grants_manage_and_is_audited(pool: PgPool) {
    use bergsmith_coordinator::identity::administration;

    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let operator = support::user(&mut conn, fixtures.organization, "operator").await;
    grants::create_grant(
        &mut conn,
        &grant(
            fixtures.organization,
            operator,
            Role::Operator,
            GrantScope::organization(),
        ),
    )
    .await
    .unwrap();
    let target = support::user(&mut conn, fixtures.organization, "target").await;

    // An operator cannot manage grants.
    let denied = administration::grant_role(
        &mut conn,
        fixtures.organization,
        operator,
        &grant(
            fixtures.organization,
            target,
            Role::Viewer,
            GrantScope::organization(),
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(denied.reason_code(), "AuthorizationDenied");

    // An administrator can, and the change is attributable.
    let created = administration::grant_role(
        &mut conn,
        fixtures.organization,
        fixtures.user,
        &grant(
            fixtures.organization,
            target,
            Role::Viewer,
            GrantScope::organization(),
        ),
    )
    .await
    .unwrap();
    let revoked =
        administration::revoke_role(&mut conn, fixtures.organization, fixtures.user, created.id)
            .await
            .unwrap();
    assert!(revoked.revoked_at.is_some());

    let entries = audit_ledger::list(&mut conn, fixtures.organization, None, 100)
        .await
        .unwrap();
    let granted = entries
        .iter()
        .find(|entry| entry.action == AuditAction::GrantCreated.as_str())
        .unwrap();
    assert_eq!(granted.actor_principal_id, Some(fixtures.user));
    assert_eq!(granted.resource_id, Some(created.id.to_string()));
    assert!(
        entries
            .iter()
            .any(|entry| entry.action == AuditAction::GrantRevoked.as_str())
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn identity_provisioning_is_administrative_and_audited(pool: PgPool) {
    use bergsmith_coordinator::identity::administration;

    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let operator = support::user(&mut conn, fixtures.organization, "operator").await;
    grants::create_grant(
        &mut conn,
        &grant(
            fixtures.organization,
            operator,
            Role::Operator,
            GrantScope::organization(),
        ),
    )
    .await
    .unwrap();

    let denied = administration::provision_principal(
        &mut conn,
        fixtures.organization,
        operator,
        &NewPrincipal {
            id: PrincipalId::generate(),
            organization_id: fixtures.organization,
            kind: PrincipalKind::User,
            issuer: "https://idp.example".to_owned(),
            subject: "carol".to_owned(),
            display_name: "Carol".to_owned(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(denied.reason_code(), "AuthorizationDenied");

    administration::provision_principal(
        &mut conn,
        fixtures.organization,
        fixtures.user,
        &NewPrincipal {
            id: PrincipalId::generate(),
            organization_id: fixtures.organization,
            kind: PrincipalKind::User,
            issuer: "https://idp.example".to_owned(),
            subject: "bob".to_owned(),
            display_name: "Bob".to_owned(),
        },
    )
    .await
    .unwrap();

    let provisioned = audit_ledger::list(&mut conn, fixtures.organization, None, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|entry| entry.action == AuditAction::IdentityProvisioned.as_str())
        .expect("provisioning is audited");
    assert_eq!(provisioned.actor_principal_id, Some(fixtures.user));
    assert_eq!(provisioned.detail["issuer"], "https://idp.example");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn administrative_session_revocation_terminates_sessions(pool: PgPool) {
    use bergsmith_coordinator::identity::administration;

    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let config = SessionConfig {
        secure: false,
        ..SessionConfig::default()
    };
    let issued = session::issue(
        &mut conn,
        &config,
        fixtures.organization,
        fixtures.user,
        "https://idp.example",
        "alice",
    )
    .await
    .unwrap();

    assert_eq!(
        administration::revoke_sessions(
            &mut conn,
            fixtures.organization,
            fixtures.user,
            fixtures.user,
        )
        .await
        .unwrap(),
        1
    );
    assert!(
        session::authenticate(&mut conn, &config, &issued.token)
            .await
            .is_err()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn bootstrap_provisions_an_administrator_idempotently(pool: PgPool) {
    use bergsmith_coordinator::database::{self, BootstrapAdministrator};

    let mut conn = pool.acquire().await.unwrap();
    let organization = support::organization(&mut conn, "acme").await;
    let request = BootstrapAdministrator {
        organization: "acme".to_owned(),
        issuer: "https://idp.example".to_owned(),
        subject: "admin".to_owned(),
        display_name: "First admin".to_owned(),
    };

    let first = database::bootstrap_administrator(&mut conn, &request)
        .await
        .unwrap();
    let second = database::bootstrap_administrator(&mut conn, &request)
        .await
        .unwrap();
    assert_eq!(first, second, "bootstrap is idempotent");

    let grants = Authorizer::grants(&mut conn, organization, first)
        .await
        .unwrap();
    assert_eq!(grants.len(), 1, "no duplicate grant is created");
    assert_eq!(grants[0].role, Role::Administrator);
    assert!(grants[0].scope.is_organization());

    let entries = audit_ledger::list(&mut conn, organization, None, 100)
        .await
        .unwrap();
    let actions: Vec<&str> = entries.iter().map(|entry| entry.action.as_str()).collect();
    assert!(actions.contains(&AuditAction::IdentityProvisioned.as_str()));
    assert!(actions.contains(&AuditAction::GrantCreated.as_str()));
    assert!(
        entries
            .iter()
            .all(|entry| entry.actor_kind != ActorKind::User),
        "bootstrap is a system action"
    );
}

/// End-to-end sign-in against a mock OIDC provider, driven through the real
/// `/auth` router.
mod oidc_flow {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use bergsmith_common::ids::{OrganizationId, PrincipalId};
    use bergsmith_common::secret::Secret;
    use bergsmith_coordinator::identity::oidc::{OidcConfig, OidcProvider, TokenEndpointAuth};
    use bergsmith_coordinator::identity::routes::{
        CSRF_COOKIE, IdentityState, LOGIN_COOKIE, router,
    };
    use bergsmith_coordinator::identity::session::SessionConfig;
    use bergsmith_persistence::Database;
    use bergsmith_persistence::tenancy::{self, NewPrincipal, PrincipalKind};
    use chrono::Utc;
    use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
    use serde_json::{Value, json};
    use tower::ServiceExt as _;
    use url::Url;

    use super::*;

    /// A fixed 2048-bit test key. Test-only; never a deployment key.
    const PKCS8_B64: &str = "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC4zkZhFRe4rrhqYA3jzRdPp7XZrAu2N0NQXtDn4wyvKyJ8AWSseK91s5hmSaSZbmOfg+vAt/bzYFH1M9HcSxNfmMBbpzScAZXLgXNkZdJ3xxThqRGqkrZQ8l8JtibTZXleBfG8ofs/jZd3KxkuOUza1ywnWZrfnILedPwkv338cflYEv2eBV4Hw3pOW1izum2SY/i16tT24TtKu5QK99qPYON6Sj6sXn6DhjCgw1ZrCRqh0kixBurwFP6FSELHoy9gT5QhomX/psi9Rry9gP+G6j9DMLIN9UtO9vTb1vMMLnat/YEQ5R2Hxf8MRDgshbMnGI9P5ULYiCH24Xm7elMHAgMBAAECggEAIVC6Wk0JojcJMtAPIPZ7kdYLj+qWj+pFfcsiAPOq+Tce99juc7Mr+l1+h04WWLyQRBTtnqfrYnOGQtxtJDf6jZzFbKBneQpGHVJAYaSDfSlMB+RJNYVOTahr3p3SQQDFkOAkdJuE5i+Z0xdXakBVLQxmPs/F/jLCWuUfAWbuCOCaQcNuI7oHvK3mO8SR7PPruNa9eZmX7CfEo2xs6llJXEapg3biQGNyIAK44zYPCHpRoV/nu1CayFetznt8pMybXIJjjw9XN4+QoGS4PHB1Ak2NURJcnXcDGYk3tbiIQtsWkBrVRsNaJvojwhhg98TQ5fkULyaqm09kBaTsPimFBQKBgQDwOz96Wy6E+G/4mvTrYymsil5tXSYIA4zILXVApZzvXtIe6yxTOhTZOu3nAOVECeZRMAkxPyhXO7bidAiX049JveOcx6z/UnXcWGRxoF59QnNMoEJQibtEgSTs8g6SN1k/g+eCRaYtwTIjCNtQnmI2HanqhJ+CjVTb1WggHSCquwKBgQDE760hJMgaeM3x+K2Jw8Hk1+h1bAI6zafKVf11c9jLzUMjowK14yoTHDi2Ar20dRs+dbp2kxy1a+unjbG0i9kK3m/+uwYctsXxaUOoNUpvDYjBrJzvxcgEeT4CbrZiFGkwo1gK9WYSRnqqOa+VyWf044NgetJDsNXgXWjidnmSJQKBgQCPiIbb32EDF8EnA8a0YXJD4gSbSV1wylYyQF7fOlRy/O+Eg7TfIUPVqwz7yaAyL8wJ1C6Z2weKX2ZdFUERHdluEOPPB8rIXoeHGNUiwtPR+YMUVGhq4HAgdR0P6WxlVmafuXr4bg7lJZqELkWDfK5ncP/V3FnF0hXSp0ZocZ7UAwKBgEiTa2JDg9r9migKVSnoEpSbXPepDklNcQCIIBgg0UyKIKwJbv5I8UCJrfyllfPjFF84U7dPD81sMDRXa531y87NmshtdBsw4FHPUTe0K/IxMqztSAi1ArljVQ7gbiZHD1z1VYT0WuFxR9Nj3ul8h+sfzVLI8KQxjyBYTEbpEnRhAoGBAOpHi+06eB4UyNcnG2kHGST7vIhmbLapkwmCVn05V0GNf4F+Pzufu0+Fza6jE6DNlTVBt6HOMvHywh9q/4/3DDJXts0guCSNc5Yn1UMXDJFOpSqXZ9LIrHeMBpZWGii5JhDy8CxfPokXSTPSnN1sKLypEhrWLopzbjguXuoxhQw9";
    const N_B64: &str = "uM5GYRUXuK64amAN480XT6e12awLtjdDUF7Q5-MMrysifAFkrHivdbOYZkmkmW5jn4PrwLf282BR9TPR3EsTX5jAW6c0nAGVy4FzZGXSd8cU4akRqpK2UPJfCbYm02V5XgXxvKH7P42XdysZLjlM2tcsJ1ma35yC3nT8JL99_HH5WBL9ngVeB8N6TltYs7ptkmP4terU9uE7SruUCvfaj2Djeko-rF5-g4YwoMNWawkaodJIsQbq8BT-hUhCx6MvYE-UIaJl_6bIvUa8vYD_huo_QzCyDfVLTvb029bzDC52rf2BEOUdh8X_DEQ4LIWzJxiPT-VC2Igh9uF5u3pTBw";
    const E_B64: &str = "AQAB";

    /// Builds an RS256 ID token bound to `nonce` and signed by the test key.
    fn id_token(issuer: &str, audience: &str, nonce: &str) -> String {
        id_token_named(issuer, audience, nonce, None)
    }

    /// Builds an ID token with an optional `name` claim.
    fn id_token_named(issuer: &str, audience: &str, nonce: &str, name: Option<&str>) -> String {
        let der = BASE64_STANDARD.decode(PKCS8_B64).unwrap();
        let key = RsaKeyPair::from_pkcs8(&der).unwrap();
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","kid":"test","typ":"JWT"}"#);
        let now = Utc::now().timestamp();
        let mut claims = json!({
            "iss": issuer,
            "sub": "alice",
            "aud": audience,
            "exp": now + 600,
            "iat": now,
            "nonce": nonce,
        });
        if let Some(name) = name {
            claims["name"] = json!(name);
        }
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        let signing_input = format!("{header}.{claims}");
        let mut signature = vec![0; key.public().modulus_len()];
        key.sign(
            &RSA_PKCS1_SHA256,
            &ring::rand::SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signature,
        )
        .unwrap();
        format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    fn jwks() -> String {
        json!({
            "keys": [{
                "kty": "RSA",
                "kid": "test",
                "use": "sig",
                "alg": "RS256",
                "n": N_B64,
                "e": E_B64,
            }]
        })
        .to_string()
    }

    fn set_cookie(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
        headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(|value| {
                let (key, rest) = value.split_once('=')?;
                if key == name {
                    Some(rest.split(';').next()?.to_owned())
                } else {
                    None
                }
            })
    }

    fn query_param(location: &str, key: &str) -> String {
        Url::parse(location)
            .unwrap()
            .query_pairs()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.into_owned())
            .unwrap()
    }

    async fn get(app: &Router, uri: &str) -> axum::response::Response {
        app.clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn provider(server: &mut mockito::ServerGuard) -> Arc<OidcProvider> {
        let issuer = server.url();
        server
            .mock("GET", "/.well-known/openid-configuration")
            .with_body(
                json!({
                    "issuer": issuer,
                    "authorization_endpoint": format!("{issuer}/authorize"),
                    "token_endpoint": format!("{issuer}/token"),
                    "jwks_uri": format!("{issuer}/jwks"),
                })
                .to_string(),
            )
            .create_async()
            .await;
        let config = OidcConfig {
            issuer: issuer.clone(),
            client_id: "bergsmith".to_owned(),
            client_secret: Some(Secret::new("secret")),
            redirect_uri: Url::parse("http://127.0.0.1:3000/auth/callback").unwrap(),
            scopes: Vec::new(),
            token_endpoint_auth: TokenEndpointAuth::ClientSecretBasic,
            allow_insecure_http: true,
            leeway: Duration::from_secs(60),
            request_timeout: Duration::from_secs(5),
            http: reqwest::Client::new(),
        };
        Arc::new(OidcProvider::discover(config).await.unwrap())
    }

    async fn provision(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        issuer: &str,
    ) -> PrincipalId {
        provision_named(conn, organization_id, issuer, "Alice").await
    }

    async fn provision_named(
        conn: &mut sqlx::PgConnection,
        organization_id: OrganizationId,
        issuer: &str,
        display_name: &str,
    ) -> PrincipalId {
        let id = PrincipalId::generate();
        tenancy::create_principal(
            conn,
            &NewPrincipal {
                id,
                organization_id,
                kind: PrincipalKind::User,
                issuer: issuer.to_owned(),
                subject: "alice".to_owned(),
                display_name: display_name.to_owned(),
            },
        )
        .await
        .unwrap();
        id
    }

    /// Runs the authorization-code flow against the mock provider and returns
    /// the callback response. The token and JWKS mocks are removed afterwards
    /// so a later sign-in in the same test can present different claims.
    async fn sign_in(
        app: &Router,
        server: &mut mockito::ServerGuard,
        issuer: &str,
        name: Option<&str>,
    ) -> axum::response::Response {
        let response = get(app, "/auth/login?organization=acme").await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let login = set_cookie(response.headers(), LOGIN_COOKIE).unwrap();
        let state_value = query_param(&location, "state");
        let nonce = query_param(&location, "nonce");
        let token = server
            .mock("POST", "/token")
            .with_body(
                json!({ "id_token": id_token_named(issuer, "bergsmith", &nonce, name) })
                    .to_string(),
            )
            .create_async()
            .await;
        let jwks = server
            .mock("GET", "/jwks")
            .with_body(jwks())
            .create_async()
            .await;
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/auth/callback?code=abc&state={state_value}"))
                    .header(header::COOKIE, format!("{LOGIN_COOKIE}={login}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        token.remove_async().await;
        jwks.remove_async().await;
        response
    }

    /// Waits until a backend in this test's database is blocked on a lock.
    async fn wait_for_row_lock(pool: &PgPool) -> bool {
        for _ in 0..500 {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(
                     SELECT 1 FROM pg_stat_activity
                     WHERE datname = current_database()
                       AND wait_event_type = 'Lock'
                       AND state = 'active')",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// A sign-in that reads the principal while a deactivation holds the row
    /// waits for it, then observes the deactivated row and is refused. The
    /// session share lock is what prevents a session from being inserted after
    /// deactivation revoked the earlier ones.
    #[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
    async fn a_sign_in_that_races_a_deactivation_is_refused(pool: PgPool) {
        let mut server = mockito::Server::new_async().await;
        let provider = provider(&mut server).await;
        let issuer = server.url();

        let mut conn = pool.acquire().await.unwrap();
        let organization = support::organization(&mut conn, "acme").await;
        let principal = provision(&mut conn, organization, &issuer).await;
        drop(conn);

        let state = IdentityState::new(
            Database::from_pool(pool.clone()),
            provider,
            SessionConfig {
                secure: false,
                ..SessionConfig::default()
            },
        );
        let app = router(state);

        // A deactivation in progress holds the principal row.
        let mut blocking = pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM principals WHERE id = $1 FOR UPDATE")
            .bind(principal.as_uuid())
            .execute(&mut *blocking)
            .await
            .unwrap();

        let response = get(&app, "/auth/login?organization=acme").await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let login = set_cookie(response.headers(), LOGIN_COOKIE).unwrap();
        let state_value = query_param(&location, "state");
        let nonce = query_param(&location, "nonce");
        let token = server
            .mock("POST", "/token")
            .with_body(json!({ "id_token": id_token(&issuer, "bergsmith", &nonce) }).to_string())
            .create_async()
            .await;
        let jwks = server
            .mock("GET", "/jwks")
            .with_body(jwks())
            .create_async()
            .await;

        let callback = tokio::spawn({
            let app = app.clone();
            let uri = format!("/auth/callback?code=abc&state={state_value}");
            let cookie = format!("{LOGIN_COOKIE}={login}");
            async move {
                app.oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(header::COOKIE, cookie)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        });
        assert!(
            wait_for_row_lock(&pool).await,
            "the sign-in must wait for the principal row"
        );

        // The deactivation commits while the sign-in waits.
        sqlx::query("UPDATE principals SET deactivated_at = now() WHERE id = $1")
            .bind(principal.as_uuid())
            .execute(&mut *blocking)
            .await
            .unwrap();
        blocking.commit().await.unwrap();

        let response = callback.await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        token.remove_async().await;
        jwks.remove_async().await;
        let sessions: i64 =
            sqlx::query_scalar("SELECT count(*) FROM auth_sessions WHERE principal_id = $1")
                .bind(principal.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(sessions, 0, "a refused sign-in leaves no session behind");
    }

    #[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
    async fn a_sign_in_refreshes_the_display_name_and_refuses_a_deactivated_principal(
        pool: PgPool,
    ) {
        let mut server = mockito::Server::new_async().await;
        let provider = provider(&mut server).await;
        let issuer = server.url();

        let mut conn = pool.acquire().await.unwrap();
        let organization = support::organization(&mut conn, "acme").await;
        let principal = provision_named(&mut conn, organization, &issuer, "Stale").await;
        drop(conn);

        let state = IdentityState::new(
            Database::from_pool(pool.clone()),
            provider,
            SessionConfig {
                secure: false,
                ..SessionConfig::default()
            },
        );
        let app = router(state);

        let response = sign_in(&app, &mut server, &issuer, Some("Alice Cooper")).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let mut conn = pool.acquire().await.unwrap();
        let stored: String =
            sqlx::query_scalar("SELECT display_name FROM principals WHERE id = $1")
                .bind(principal.as_uuid())
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(stored, "Alice Cooper");

        let mut tx = pool.begin().await.unwrap();
        tenancy::deactivate_principal(&mut tx, organization, principal)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let response = sign_in(&app, &mut server, &issuer, Some("Alice Cooper")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let reason: String = sqlx::query_scalar(
            "SELECT detail->>'reason' FROM audit_events
             WHERE action = 'auth.login_failed'
             ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        assert_eq!(reason, "identity_deactivated");
    }

    #[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
    async fn a_full_login_issues_queries_and_revokes_a_session(pool: PgPool) {
        let mut server = mockito::Server::new_async().await;
        let provider = provider(&mut server).await;
        let issuer = server.url();

        let mut conn = pool.acquire().await.unwrap();
        let organization = support::organization(&mut conn, "acme").await;
        let principal = provision(&mut conn, organization, &issuer).await;
        drop(conn);

        let verifier = provider.clone();
        let state = IdentityState::new(
            Database::from_pool(pool.clone()),
            provider,
            SessionConfig {
                secure: false,
                ..SessionConfig::default()
            },
        );
        let app = router(state);

        let response = get(&app, "/auth/login?organization=acme&return_to=/jobs").await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let login = set_cookie(response.headers(), LOGIN_COOKIE).unwrap();
        let state_value = query_param(&location, "state");
        let nonce = query_param(&location, "nonce");

        server
            .mock("POST", "/token")
            .with_body(json!({ "id_token": id_token(&issuer, "bergsmith", &nonce) }).to_string())
            .create_async()
            .await;
        server
            .mock("GET", "/jwks")
            .with_body(jwks())
            .create_async()
            .await;

        // Fail fast with the real reason if the fixture token is invalid.
        verifier
            .verify_id_token_at(
                &id_token(&issuer, "bergsmith", &nonce),
                &nonce,
                Utc::now().timestamp(),
            )
            .await
            .expect("the fixture ID token verifies");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/auth/callback?code=abc&state={state_value}"))
                    .header(header::COOKIE, format!("{LOGIN_COOKIE}={login}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        if response.status() != StatusCode::SEE_OTHER {
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            panic!(
                "callback returned {status}: {}",
                String::from_utf8_lossy(&body)
            );
        }
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/jobs",
            "the validated return path is used"
        );
        assert_eq!(
            set_cookie(response.headers(), LOGIN_COOKIE).as_deref(),
            Some("")
        );
        let session = set_cookie(response.headers(), "bergsmith_session").expect("session cookie");
        let csrf = set_cookie(response.headers(), CSRF_COOKIE).expect("csrf cookie");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/auth/session")
                    .header(header::COOKIE, format!("bergsmith_session={session}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let described: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(described["principal_id"], principal.to_string());

        // A wrong CSRF value is denied while still attributed to the
        // authenticated principal.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/logout")
                    .header(
                        header::COOKIE,
                        format!("bergsmith_session={session}; {CSRF_COOKIE}={csrf}"),
                    )
                    .header("x-csrf-token", "wrong-csrf")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // A malformed session cookie is audited too, even though it never
        // reaches the validity check.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/auth/session")
                    .header(header::COOKIE, "bergsmith_session=short")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/auth/logout")
                    .header(
                        header::COOKIE,
                        format!("bergsmith_session={session}; {CSRF_COOKIE}={csrf}"),
                    )
                    .header("x-csrf-token", csrf.clone())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // The revoked session no longer authenticates, and the denial is
        // attributable.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/auth/session")
                    .header(header::COOKIE, format!("bergsmith_session={session}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let mut conn = pool.acquire().await.unwrap();
        let entries = audit_ledger::list(&mut conn, organization, None, 100)
            .await
            .unwrap();
        let denials: Vec<_> = entries
            .iter()
            .filter(|entry| entry.action == "auth.session_denied")
            .collect();
        let csrf_denial = denials
            .iter()
            .find(|entry| entry.detail["reason"] == "invalid_csrf")
            .expect("the CSRF denial is audited");
        assert_eq!(csrf_denial.actor_principal_id, Some(principal));
        assert_eq!(csrf_denial.actor_kind, ActorKind::User);
        // A malformed cookie cannot name an organization, so its denial is
        // recorded without one and is anonymous.
        let (organization_id, actor, kind): (Option<uuid::Uuid>, Option<uuid::Uuid>, String) =
            sqlx::query_as(
                "SELECT organization_id, actor_principal_id, actor_kind
                 FROM audit_events
                 WHERE action = 'auth.session_denied'
                   AND detail->>'reason' = 'malformed_session'
                 ORDER BY id DESC LIMIT 1",
            )
            .fetch_one(&mut *conn)
            .await
            .expect("the malformed cookie is audited");
        assert_eq!(organization_id, None);
        assert_eq!(actor, None);
        assert_eq!(kind, "Anonymous");
        assert!(
            denials
                .iter()
                .any(|entry| entry.detail["reason"] == "invalid_session"),
            "the revoked session is audited"
        );
    }

    #[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
    async fn a_captured_state_cannot_be_redeemed_in_another_browser(pool: PgPool) {
        let mut server = mockito::Server::new_async().await;
        let provider = provider(&mut server).await;

        let mut conn = pool.acquire().await.unwrap();
        let organization = support::organization(&mut conn, "acme").await;
        provision(&mut conn, organization, &server.url()).await;
        drop(conn);

        let state = IdentityState::new(
            Database::from_pool(pool.clone()),
            provider,
            SessionConfig {
                secure: false,
                ..SessionConfig::default()
            },
        );
        let app = router(state);

        let response = get(&app, "/auth/login?organization=acme").await;
        let location = response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let state_value = query_param(&location, "state");

        // The attacker captured the state but presents a different binding
        // cookie, so the exchange is refused before any provider call.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/auth/callback?code=abc&state={state_value}"))
                    .header(header::COOKIE, format!("{LOGIN_COOKIE}=not-the-binding"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // And open-redirect attempts are refused at login, including the
        // percent-encoded backslash the query extractor decodes.
        for return_to in ["/%5Cevil.example", "/\\evil.example", "//evil.example"] {
            let response = get(
                &app,
                &format!("/auth/login?organization=acme&return_to={return_to}"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{return_to}");
        }
    }
    #[tokio::test]
    async fn client_secret_basic_form_encodes_credentials() {
        let mut server = mockito::Server::new_async().await;
        let issuer = server.url();
        server
            .mock("GET", "/.well-known/openid-configuration")
            .with_body(
                json!({
                    "issuer": issuer,
                    "authorization_endpoint": format!("{issuer}/authorize"),
                    "token_endpoint": format!("{issuer}/token"),
                    "jwks_uri": format!("{issuer}/jwks"),
                })
                .to_string(),
            )
            .create_async()
            .await;
        // RFC 6749: id `id +%` encodes to `id+%2B%25`, secret `s:e c` to
        // `s%3Ae+c`.
        let expected = format!("Basic {}", BASE64_STANDARD.encode("id+%2B%25:s%3Ae+c"));
        let token = server
            .mock("POST", "/token")
            .match_header("authorization", expected.as_str())
            .with_body(json!({ "id_token": id_token(&issuer, "id +%", "nonce") }).to_string())
            .create_async()
            .await;
        server
            .mock("GET", "/jwks")
            .with_body(jwks())
            .create_async()
            .await;
        let config = OidcConfig {
            issuer: issuer.clone(),
            client_id: "id +%".to_owned(),
            client_secret: Some(Secret::new("s:e c")),
            redirect_uri: Url::parse("http://127.0.0.1:3000/auth/callback").unwrap(),
            scopes: Vec::new(),
            token_endpoint_auth: TokenEndpointAuth::ClientSecretBasic,
            allow_insecure_http: true,
            leeway: Duration::from_secs(60),
            request_timeout: Duration::from_secs(5),
            http: reqwest::Client::new(),
        };
        let provider = OidcProvider::discover(config).await.unwrap();
        provider
            .exchange_code("code", &Secret::new("verifier"), "nonce")
            .await
            .unwrap();
        token.assert_async().await;
    }
}

/// Waits until a backend is blocked on an advisory lock, so a test can order a
/// change against a request that has already started.
async fn wait_for_advisory_waiter(pool: &PgPool) -> bool {
    for _ in 0..500 {
        let waiting: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                 SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND NOT granted)",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        if waiting {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    false
}

/// An administrator cannot reactivate themselves against the state a
/// concurrent deactivation has already committed.
///
/// The lifecycle lock is acquired before authorization, so the reactivation
/// waits, then sees the actor deactivated and is refused. Without that
/// ordering the reactivation would have authorized while the actor was still
/// active and undone the deactivation.
#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_deactivation_holding_the_lifecycle_lock_refuses_a_self_reactivation(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let other = support::user(&mut conn, tenant.organization, "bob").await;
    grants::create_grant(
        &mut conn,
        &grant(
            tenant.organization,
            other,
            Role::Administrator,
            GrantScope::organization(),
        ),
    )
    .await
    .unwrap();
    drop(conn);

    let mut deactivating = pool.begin().await.unwrap();
    tenancy::deactivate_principal(&mut deactivating, tenant.organization, tenant.user)
        .await
        .unwrap();

    let reactivating_pool = pool.clone();
    let organization = tenant.organization;
    let alice = tenant.user;
    let reactivating = tokio::spawn(async move {
        let mut conn = reactivating_pool.acquire().await.unwrap();
        administration::reactivate_principal(&mut conn, organization, alice, alice).await
    });
    assert!(
        wait_for_advisory_waiter(&pool).await,
        "the reactivation must wait for the lifecycle lock"
    );
    deactivating.commit().await.unwrap();

    let result = reactivating.await.unwrap();
    assert!(
        matches!(
            result,
            Err(administration::AdministrationError::Authorization(_))
        ),
        "the deactivated actor must not authorize its own reactivation: {result:?}"
    );
    let active: bool =
        sqlx::query_scalar("SELECT deactivated_at IS NULL FROM principals WHERE id = $1")
            .bind(alice.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!active);
}

/// Mutual deactivations serialize under the lifecycle lock: whichever runs
/// second sees the first change and is refused, so the organization never
/// loses every administrator.
#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_deactivation_cannot_consume_the_administrator_that_just_changed(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = tenant(&mut conn, "acme").await;
    let bob = support::user(&mut conn, tenant.organization, "bob").await;
    grants::create_grant(
        &mut conn,
        &grant(
            tenant.organization,
            bob,
            Role::Administrator,
            GrantScope::organization(),
        ),
    )
    .await
    .unwrap();
    drop(conn);

    // Alice deactivates Bob and holds the lock.
    let mut deactivating = pool.begin().await.unwrap();
    tenancy::deactivate_principal(&mut deactivating, tenant.organization, bob)
        .await
        .unwrap();
    // Bob's concurrent request deactivates Alice.
    let deactivating_pool = pool.clone();
    let organization = tenant.organization;
    let actor = bob;
    let target = tenant.user;
    let pending = tokio::spawn(async move {
        let mut conn = deactivating_pool.acquire().await.unwrap();
        administration::deactivate_principal(&mut conn, organization, actor, target).await
    });
    assert!(
        wait_for_advisory_waiter(&pool).await,
        "the second deactivation must wait for the lifecycle lock"
    );
    deactivating.commit().await.unwrap();

    let result = pending.await.unwrap();
    assert!(
        matches!(
            result,
            Err(administration::AdministrationError::Authorization(_))
        ),
        "the deactivated actor must not deactivate another administrator: {result:?}"
    );
    let active_administrators: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM principals p
         JOIN principal_role_grants g
           ON g.organization_id = p.organization_id AND g.principal_id = p.id
         WHERE p.organization_id = $1 AND p.deactivated_at IS NULL
           AND g.revoked_at IS NULL AND g.role = 'Administrator'
           AND g.catalog_connection_id IS NULL AND g.table_identity_id IS NULL",
    )
    .bind(tenant.organization.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active_administrators, 1);
}
