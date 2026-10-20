//! OIDC login state, server-side sessions, scoped role grants, and the audit
//! log against real PostgreSQL (BS-017.T1–T4).
#![expect(clippy::expect_used, reason = "test fixtures fail on setup errors")]

mod support;

use bergsmith_common::audit::{ActorKind, AuditAction, AuditOutcome};
use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{PrincipalId, RoleGrantId};
use bergsmith_common::permission::{GrantScope, Permission, ResourceScope, Role};
use bergsmith_common::secret::Secret;
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::audit::{self, NewAuditEvent};
use bergsmith_persistence::auth::{self, NewLoginState, NewSession};
use bergsmith_persistence::grants::{self, NewRoleGrant};
use serde_json::json;
use sqlx::PgPool;
use support::{constraint, organization, tenant, user};

const SESSION_DOMAIN: &[u8] = b"test.session";
const CSRF_DOMAIN: &[u8] = b"test.csrf";

fn digest(domain: &[u8], value: &str) -> Sha256Digest {
    Sha256Digest::of(domain, value.as_bytes())
}

async fn open_session(
    conn: &mut sqlx::PgConnection,
    organization_id: bergsmith_common::ids::OrganizationId,
    principal_id: PrincipalId,
    marker: &str,
) -> Sha256Digest {
    let token_hash = digest(SESSION_DOMAIN, marker);
    auth::create_session(
        conn,
        &NewSession {
            token_hash,
            organization_id,
            principal_id,
            issuer: "https://idp.example".to_owned(),
            subject: "alice".to_owned(),
            csrf_token_hash: digest(CSRF_DOMAIN, marker),
            lifetime: std::time::Duration::from_secs(3_600),
        },
    )
    .await
    .expect("session is created");
    token_hash
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_session_authenticates_and_a_revoked_one_does_not(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let token_hash = open_session(
        &mut conn,
        fixtures.organization,
        fixtures.user,
        "alice-session",
    )
    .await;

    let session =
        auth::authenticate_session(&mut conn, token_hash, std::time::Duration::from_secs(600))
            .await
            .expect("fresh session authenticates");
    assert_eq!(session.principal_id, fixtures.user);
    assert_eq!(session.issuer, "https://idp.example");

    assert!(
        auth::revoke_session(&mut conn, token_hash, "logout")
            .await
            .unwrap()
    );
    let error =
        auth::authenticate_session(&mut conn, token_hash, std::time::Duration::from_secs(600))
            .await
            .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::SessionRejected { reason: "revoked" }
    ));
    // Revocation is idempotent: a second attempt changes nothing.
    assert!(
        !auth::revoke_session(&mut conn, token_hash, "logout")
            .await
            .unwrap()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn expired_and_idle_sessions_are_rejected_with_a_reason(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;

    let expired = open_session(&mut conn, fixtures.organization, fixtures.user, "expired").await;
    // Move the whole window back so `expires_at > created_at` still holds.
    sqlx::query("UPDATE auth_sessions SET created_at = clock_timestamp() - interval '2 hours', expires_at = clock_timestamp() - interval '1 hour' WHERE token_hash = $1")
        .bind(expired.as_bytes().as_slice())
        .execute(&mut *conn)
        .await
        .unwrap();
    let error = auth::authenticate_session(&mut conn, expired, std::time::Duration::from_secs(600))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::SessionRejected { reason: "expired" }
    ));

    let idle = open_session(&mut conn, fixtures.organization, fixtures.user, "idle").await;
    sqlx::query("UPDATE auth_sessions SET created_at = clock_timestamp() - interval '3 hours', last_seen_at = clock_timestamp() - interval '2 hours' WHERE token_hash = $1")
        .bind(idle.as_bytes().as_slice())
        .execute(&mut *conn)
        .await
        .unwrap();
    let error =
        auth::authenticate_session(&mut conn, idle, std::time::Duration::from_secs(60)).await;
    assert!(matches!(
        error.unwrap_err(),
        PersistenceError::SessionRejected { reason: "idle" }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_short_idle_timeout_is_refreshed_before_it_expires(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let token_hash = open_session(
        &mut conn,
        fixtures.organization,
        fixtures.user,
        "short-idle",
    )
    .await;
    // Six seconds into a ten-second idle window, a successful request must
    // refresh the timestamp; the old fixed 30-second touch did not.
    sqlx::query(
        "UPDATE auth_sessions
         SET created_at = clock_timestamp() - interval '2 minutes',
             last_seen_at = clock_timestamp() - interval '6 seconds'
         WHERE token_hash = $1",
    )
    .bind(token_hash.as_bytes().as_slice())
    .execute(&mut *conn)
    .await
    .unwrap();

    auth::authenticate_session(&mut conn, token_hash, std::time::Duration::from_secs(10))
        .await
        .expect("the session is still inside its ten-second idle window");

    let refreshed: bool = sqlx::query_scalar(
        "SELECT last_seen_at > clock_timestamp() - interval '2 seconds'
         FROM auth_sessions WHERE token_hash = $1",
    )
    .bind(token_hash.as_bytes().as_slice())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert!(
        refreshed,
        "a successful request must refresh last_seen_at before a short idle timeout elapses"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unknown_session_is_indistinguishable_from_a_missing_one(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let _ = tenant(&mut conn, "acme").await;
    let error = auth::authenticate_session(
        &mut conn,
        digest(SESSION_DOMAIN, "never-issued"),
        std::time::Duration::from_secs(600),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::SessionRejected { reason: "unknown" }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_session_requires_a_principal_with_the_bound_identity(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let token_hash = open_session(&mut conn, fixtures.organization, fixtures.user, "binding").await;
    sqlx::query("UPDATE principals SET subject = 'renamed' WHERE id = $1")
        .bind(fixtures.user.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let error =
        auth::authenticate_session(&mut conn, token_hash, std::time::Duration::from_secs(600))
            .await
            .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::SessionRejected { reason: "unknown" }
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn login_states_are_single_use(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let org = organization(&mut conn, "acme").await;
    let state_hash = digest(b"test.login-state", "state");
    auth::create_login_state(
        &mut conn,
        &NewLoginState {
            state_hash,
            binding_hash: digest(b"test.login-binding", "binding"),
            organization_id: org,
            return_to: "/tables".to_owned(),
            nonce: "nonce-value-0123456789".to_owned(),
            pkce_verifier: Secret::new("v".repeat(43)),
            lifetime: std::time::Duration::from_secs(600),
        },
    )
    .await
    .unwrap();

    let consumed = auth::consume_login_state(
        &mut conn,
        state_hash,
        digest(b"test.login-binding", "binding"),
    )
    .await
    .expect("the first exchange wins");
    assert_eq!(consumed.return_to, "/tables");
    assert_eq!(consumed.pkce_verifier.expose(), "v".repeat(43));

    let error = auth::consume_login_state(
        &mut conn,
        state_hash,
        digest(b"test.login-binding", "binding"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PersistenceError::NotFound { .. }));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_expired_login_state_is_rejected(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let org = organization(&mut conn, "acme").await;
    let state_hash = digest(b"test.login-state", "stale");
    auth::create_login_state(
        &mut conn,
        &NewLoginState {
            state_hash,
            binding_hash: digest(b"test.login-binding", "binding"),
            organization_id: org,
            return_to: "/".to_owned(),
            nonce: "nonce-value-0123456789".to_owned(),
            pkce_verifier: Secret::new("v".repeat(43)),
            lifetime: std::time::Duration::from_secs(600),
        },
    )
    .await
    .unwrap();
    sqlx::query("UPDATE auth_login_states SET created_at = clock_timestamp() - interval '2 minutes', expires_at = clock_timestamp() - interval '1 minute' WHERE state_hash = $1")
        .bind(state_hash.as_bytes().as_slice())
        .execute(&mut *conn)
        .await
        .unwrap();
    assert!(matches!(
        auth::consume_login_state(
            &mut conn,
            state_hash,
            digest(b"test.login-binding", "binding"),
        )
        .await,
        Err(PersistenceError::NotFound { .. })
    ));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_absolute_return_to_is_rejected_by_the_schema(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let org = organization(&mut conn, "acme").await;
    let error = auth::create_login_state(
        &mut conn,
        &NewLoginState {
            state_hash: digest(b"test.login-state", "open-redirect"),
            binding_hash: digest(b"test.login-binding", "open-redirect"),
            organization_id: org,
            return_to: "https://evil.example".to_owned(),
            nonce: "nonce-value-0123456789".to_owned(),
            pkce_verifier: Secret::new("v".repeat(43)),
            lifetime: std::time::Duration::from_secs(600),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PersistenceError::InvalidValue { .. }));
    assert_eq!(constraint(&error), "auth_login_states_return_to_check");
}

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

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn organization_and_connection_grants_scope_reads(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let other = tenant(&mut conn, "other").await;
    let viewer = user(&mut conn, fixtures.organization, "viewer").await;

    // Only a connection-scoped operator grant: no organization-wide grant.
    grants::create_grant(
        &mut conn,
        &grant(
            fixtures.organization,
            viewer,
            Role::Operator,
            GrantScope::connection(fixtures.connection),
        ),
    )
    .await
    .unwrap();

    // The connection grant covers its connection and its tables...
    for resource in [
        ResourceScope::Connection(fixtures.connection),
        ResourceScope::Table(fixtures.table),
    ] {
        assert!(
            grants::has_permission(
                &mut conn,
                fixtures.organization,
                viewer,
                Permission::ViewInventory,
                resource,
            )
            .await
            .unwrap()
        );
    }
    // ...but not another connection, nor the organization scope itself.
    assert!(
        !grants::has_permission(
            &mut conn,
            fixtures.organization,
            viewer,
            Permission::ViewInventory,
            ResourceScope::Connection(other.connection),
        )
        .await
        .unwrap()
    );
    assert!(
        !grants::has_permission(
            &mut conn,
            fixtures.organization,
            viewer,
            Permission::ViewInventory,
            ResourceScope::Organization,
        )
        .await
        .unwrap()
    );
    // The connection grant carries Operator's SubmitJobs over its tables only.
    assert!(
        grants::has_permission(
            &mut conn,
            fixtures.organization,
            viewer,
            Permission::SubmitJobs,
            ResourceScope::Table(fixtures.table),
        )
        .await
        .unwrap()
    );
    assert!(
        !grants::has_permission(
            &mut conn,
            fixtures.organization,
            viewer,
            Permission::SubmitJobs,
            ResourceScope::Table(other.table),
        )
        .await
        .unwrap()
    );
    // Cross-organization checks never succeed.
    assert!(
        !grants::has_permission(
            &mut conn,
            other.organization,
            viewer,
            Permission::ViewInventory,
            ResourceScope::Organization,
        )
        .await
        .unwrap()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn table_grants_scope_writes_to_their_table(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let other = tenant(&mut conn, "other").await;
    let viewer = user(&mut conn, fixtures.organization, "viewer").await;

    grants::create_grant(
        &mut conn,
        &grant(
            fixtures.organization,
            viewer,
            Role::Administrator,
            GrantScope::table(fixtures.table),
        ),
    )
    .await
    .unwrap();

    // Table-scoped administrator: administrator over its table only.
    assert!(
        grants::has_permission(
            &mut conn,
            fixtures.organization,
            viewer,
            Permission::SubmitJobs,
            ResourceScope::Table(fixtures.table),
        )
        .await
        .unwrap()
    );
    assert!(
        !grants::has_permission(
            &mut conn,
            fixtures.organization,
            viewer,
            Permission::SubmitJobs,
            ResourceScope::Table(other.table),
        )
        .await
        .unwrap()
    );
    // A table grant never reveals the connection or the organization.
    assert!(
        !grants::has_permission(
            &mut conn,
            fixtures.organization,
            viewer,
            Permission::ViewInventory,
            ResourceScope::Connection(fixtures.connection),
        )
        .await
        .unwrap()
    );
}
#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn revoking_a_grant_denies_the_next_check(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let operator = user(&mut conn, fixtures.organization, "operator").await;
    let created = grants::create_grant(
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

    grants::require_permission(
        &mut conn,
        fixtures.organization,
        operator,
        Permission::SubmitJobs,
        ResourceScope::Organization,
    )
    .await
    .unwrap();

    grants::revoke_grant(&mut conn, fixtures.organization, created.id, None)
        .await
        .unwrap();
    let error = grants::require_permission(
        &mut conn,
        fixtures.organization,
        operator,
        Permission::SubmitJobs,
        ResourceScope::Organization,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PersistenceError::PermissionDenied { .. }));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_active_grant_is_unique_and_a_revoked_one_can_be_reissued(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let scope = GrantScope::organization();
    // `tenant` already granted Administrator organization-wide, so Operator is
    // a second distinct role. A second active grant for the same role and
    // scope conflicts even though it is a different row.
    grants::create_grant(
        &mut conn,
        &grant(fixtures.organization, fixtures.user, Role::Operator, scope),
    )
    .await
    .unwrap();
    let error = grants::create_grant(
        &mut conn,
        &grant(fixtures.organization, fixtures.user, Role::Operator, scope),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PersistenceError::AlreadyExists { .. }));
    assert_eq!(constraint(&error), "principal_role_grants_one_active");
    grants::revoke_principal_grants(&mut conn, fixtures.organization, fixtures.user, None)
        .await
        .unwrap();
    grants::create_grant(
        &mut conn,
        &grant(fixtures.organization, fixtures.user, Role::Operator, scope),
    )
    .await
    .unwrap();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_grant_never_names_both_a_connection_and_a_table(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let error = grants::create_grant(
        &mut conn,
        &grant(
            fixtures.organization,
            fixtures.user,
            Role::Viewer,
            GrantScope {
                catalog_connection: Some(fixtures.connection),
                table: Some(fixtures.table),
            },
        ),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PersistenceError::InvalidValue { .. }));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn audit_records_are_append_only_and_secret_free(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let id = audit::record(
        &mut conn,
        &NewAuditEvent {
            organization_id: Some(fixtures.organization),
            actor_principal_id: Some(fixtures.user),
            actor_kind: ActorKind::User,
            action: AuditAction::JobPublished.as_str(),
            resource_type: "job",
            resource_id: Some("abc"),
            outcome: AuditOutcome::Allowed,
            detail: json!({ "reason": "CommitConfirmed" }),
        },
    )
    .await
    .unwrap();
    assert!(id > 0);

    let update = sqlx::query("UPDATE audit_events SET action = 'tampered' WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await
        .unwrap_err();
    assert_eq!(
        update.as_database_error().unwrap().code().as_deref(),
        Some("23001")
    );

    let error = audit::record(
        &mut conn,
        &NewAuditEvent {
            organization_id: Some(fixtures.organization),
            actor_principal_id: Some(fixtures.user),
            actor_kind: ActorKind::User,
            action: AuditAction::AuthLoginSucceeded.as_str(),
            resource_type: "session",
            resource_id: None,
            outcome: AuditOutcome::Allowed,
            detail: json!({ "access_token": "leaked" }),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PersistenceError::InvalidValue { .. }));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn audit_listing_pages_in_identifier_order(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    for action in [AuditAction::JobAdmitted, AuditAction::JobPublished] {
        audit::record(
            &mut conn,
            &NewAuditEvent {
                organization_id: Some(fixtures.organization),
                actor_principal_id: Some(fixtures.user),
                actor_kind: ActorKind::User,
                action: action.as_str(),
                resource_type: "job",
                resource_id: None,
                outcome: AuditOutcome::Allowed,
                detail: json!({}),
            },
        )
        .await
        .unwrap();
    }
    let page = audit::list(&mut conn, fixtures.organization, None, 10)
        .await
        .unwrap();
    assert_eq!(page.len(), 2);
    assert_eq!(page[0].action, AuditAction::JobAdmitted.as_str());
    let next = audit::list(&mut conn, fixtures.organization, Some(page[0].id), 10)
        .await
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].action, AuditAction::JobPublished.as_str());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn grants_carry_creation_and_revocation_times(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    let grant = grants::active_grants(&mut conn, fixtures.organization, fixtures.user)
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("the tenant user has an administrator grant");
    assert!(grant.revoked_at.is_none());
    let revoked = grants::revoke_grant(
        &mut conn,
        fixtures.organization,
        grant.id,
        Some(fixtures.user),
    )
    .await
    .unwrap();
    assert!(revoked.revoked_at.is_some());
    assert_eq!(revoked.revoked_by, Some(fixtures.user));
    assert!(revoked.granted_at <= revoked.revoked_at.expect("revocation records a time"));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_login_state_is_bound_to_the_starting_browser(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let org = organization(&mut conn, "acme").await;
    let state_hash = digest(b"test.login-state", "bound");
    let started = digest(b"test.login-binding", "started");
    auth::create_login_state(
        &mut conn,
        &NewLoginState {
            state_hash,
            binding_hash: started,
            organization_id: org,
            return_to: "/".to_owned(),
            nonce: "nonce-value-0123456789".to_owned(),
            pkce_verifier: Secret::new("v".repeat(43)),
            lifetime: std::time::Duration::from_secs(600),
        },
    )
    .await
    .unwrap();

    // Another browser presenting the captured state is rejected, and the state
    // survives for the browser that started it.
    let error = auth::consume_login_state(
        &mut conn,
        state_hash,
        digest(b"test.login-binding", "attacker"),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PersistenceError::NotFound { .. }));
    auth::consume_login_state(&mut conn, state_hash, started)
        .await
        .expect("the starting browser may redeem it");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn consuming_a_login_state_erases_the_pkce_verifier(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let org = organization(&mut conn, "acme").await;
    let state_hash = digest(b"test.login-state", "erase");
    let binding = digest(b"test.login-binding", "erase");
    auth::create_login_state(
        &mut conn,
        &NewLoginState {
            state_hash,
            binding_hash: binding,
            organization_id: org,
            return_to: "/".to_owned(),
            nonce: "nonce-value-0123456789".to_owned(),
            pkce_verifier: Secret::new("v".repeat(43)),
            lifetime: std::time::Duration::from_secs(600),
        },
    )
    .await
    .unwrap();
    auth::consume_login_state(&mut conn, state_hash, binding)
        .await
        .unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM auth_login_states WHERE state_hash = $1")
            .bind(state_hash.as_bytes().as_slice())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    assert_eq!(remaining, 0, "the verifier must not outlive consumption");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn outstanding_login_states_are_bounded_per_organization(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let org = organization(&mut conn, "acme").await;
    for index in 0..auth::MAX_OUTSTANDING_LOGIN_STATES {
        auth::create_login_state(
            &mut conn,
            &NewLoginState {
                state_hash: digest(b"test.login-state", &format!("bounded-{index}")),
                binding_hash: digest(b"test.login-binding", &format!("bounded-{index}")),
                organization_id: org,
                return_to: "/".to_owned(),
                nonce: "nonce-value-0123456789".to_owned(),
                pkce_verifier: Secret::new("v".repeat(43)),
                lifetime: std::time::Duration::from_secs(600),
            },
        )
        .await
        .unwrap();
    }
    let error = auth::create_login_state(
        &mut conn,
        &NewLoginState {
            state_hash: digest(b"test.login-state", "one-too-many"),
            binding_hash: digest(b"test.login-binding", "one-too-many"),
            organization_id: org,
            return_to: "/".to_owned(),
            nonce: "nonce-value-0123456789".to_owned(),
            pkce_verifier: Secret::new("v".repeat(43)),
            lifetime: std::time::Duration::from_secs(600),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PersistenceError::RateLimited { .. }));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn expired_sessions_are_reclaimed(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let fixtures = tenant(&mut conn, "acme").await;
    open_session(&mut conn, fixtures.organization, fixtures.user, "reclaim").await;
    sqlx::query("UPDATE auth_sessions SET created_at = clock_timestamp() - interval '2 hours', expires_at = clock_timestamp() - interval '1 hour'")
        .execute(&mut *conn)
        .await
        .unwrap();
    let reclaimed =
        auth::reclaim_expired_sessions(&mut conn, std::time::Duration::from_secs(0), 100)
            .await
            .unwrap();
    assert_eq!(reclaimed, 1);
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_sessions")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_schema_rejects_a_backslash_return_to(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let org = organization(&mut conn, "acme").await;
    let error = auth::create_login_state(
        &mut conn,
        &NewLoginState {
            state_hash: digest(b"test.login-state", "backslash"),
            binding_hash: digest(b"test.login-binding", "backslash"),
            organization_id: org,
            return_to: "/\\evil.example".to_owned(),
            nonce: "nonce-value-0123456789".to_owned(),
            pkce_verifier: Secret::new("v".repeat(43)),
            lifetime: std::time::Duration::from_secs(600),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PersistenceError::InvalidValue { .. }));
    assert_eq!(constraint(&error), "auth_login_states_return_to_check");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_grant_cannot_authorize_another_organizations_resources(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let acme = tenant(&mut conn, "acme").await;
    let globex = tenant(&mut conn, "globex").await;

    // acme's administrator holds an organization-wide grant; it must never
    // cover a resource whose identifier belongs to globex.
    assert!(
        !grants::has_permission(
            &mut conn,
            acme.organization,
            acme.user,
            Permission::ViewInventory,
            ResourceScope::Table(globex.table),
        )
        .await
        .unwrap()
    );
    assert!(
        !grants::has_permission(
            &mut conn,
            acme.organization,
            acme.user,
            Permission::ManageConnections,
            ResourceScope::Connection(globex.connection),
        )
        .await
        .unwrap()
    );
    // Its own resources still authorize.
    assert!(
        grants::has_permission(
            &mut conn,
            acme.organization,
            acme.user,
            Permission::ViewInventory,
            ResourceScope::Table(acme.table),
        )
        .await
        .unwrap()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn concurrent_logins_cannot_exceed_the_outstanding_cap(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let org = organization(&mut conn, "acme").await;
    for index in 0..(auth::MAX_OUTSTANDING_LOGIN_STATES - 1) {
        auth::create_login_state(
            &mut conn,
            &NewLoginState {
                state_hash: digest(b"test.login-state", &format!("cap-{index}")),
                binding_hash: digest(b"test.login-binding", &format!("cap-{index}")),
                organization_id: org,
                return_to: "/".to_owned(),
                nonce: "nonce-value-0123456789".to_owned(),
                pkce_verifier: Secret::new("v".repeat(43)),
                lifetime: std::time::Duration::from_secs(600),
            },
        )
        .await
        .unwrap();
    }
    let first = NewLoginState {
        state_hash: digest(b"test.login-state", "race-a"),
        binding_hash: digest(b"test.login-binding", "race-a"),
        organization_id: org,
        return_to: "/".to_owned(),
        nonce: "nonce-value-0123456789".to_owned(),
        pkce_verifier: Secret::new("v".repeat(43)),
        lifetime: std::time::Duration::from_secs(600),
    };
    let second = NewLoginState {
        state_hash: digest(b"test.login-state", "race-b"),
        binding_hash: digest(b"test.login-binding", "race-b"),
        organization_id: org,
        return_to: "/".to_owned(),
        nonce: "nonce-value-0123456789".to_owned(),
        pkce_verifier: Secret::new("v".repeat(43)),
        lifetime: std::time::Duration::from_secs(600),
    };
    let mut other = pool.acquire().await.unwrap();
    let (first, second) = tokio::join!(
        auth::create_login_state(&mut conn, &first),
        auth::create_login_state(&mut other, &second),
    );
    assert_eq!(
        usize::from(first.is_ok()) + usize::from(second.is_ok()),
        1,
        "only one login may take the last slot: {first:?} {second:?}"
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM auth_login_states
         WHERE organization_id = $1 AND expires_at > now()",
    )
    .bind(org.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(count, auth::MAX_OUTSTANDING_LOGIN_STATES);
}
