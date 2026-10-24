//! Recording identity, grant, and maintenance decisions.
//!
//! Every helper takes typed actions and outcomes. The detail object is built
//! here from identifiers and stable reasons, so callers cannot accidentally
//! attach a token, cookie, or catalog secret to an audit row.

use bergsmith_common::audit::{ActorKind, AuditAction, AuditOutcome};
use bergsmith_common::ids::{OrganizationId, PrincipalId};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::audit::{self, NewAuditEvent};
use bergsmith_persistence::tenancy::Principal;
use serde_json::{Value, json};
use sqlx::PgConnection;
use thiserror::Error;

/// An audit write failed.
///
/// Privileged commands (grants, provisioning, session revocation) treat this
/// as fatal so a change is never applied without a record; best-effort job
/// decisions log it and continue, because the decision is already durable.
#[derive(Debug, Error)]
pub enum AuditError {
    /// The append-only log rejected or could not accept the record.
    #[error("audit record could not be written")]
    Persistence(#[from] PersistenceError),
}

impl AuditError {
    /// A stable reason code for operator logs.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        "AuditUnavailable"
    }
}

impl From<AuditError> for PersistenceError {
    fn from(error: AuditError) -> Self {
        match error {
            AuditError::Persistence(source) => source,
        }
    }
}

/// Resolves the kind of an acting principal for accurate audit attribution.
///
/// A missing actor is the coordinator itself (`System`); a principal is a
/// person or a service account, so a schedule's maintenance identity is not
/// recorded as a user.
async fn actor_kind(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: Option<PrincipalId>,
) -> ActorKind {
    let Some(actor) = actor else {
        return ActorKind::System;
    };
    let stored: Result<Option<String>, _> =
        sqlx::query_scalar("SELECT kind FROM principals WHERE organization_id = $1 AND id = $2")
            .bind(organization_id.as_uuid())
            .bind(actor.as_uuid())
            .fetch_optional(&mut *conn)
            .await;
    match stored.ok().flatten().as_deref() {
        Some("Service") => ActorKind::Service,
        _ => ActorKind::User,
    }
}

/// Appends one audit record.
///
/// # Errors
///
/// [`AuditError`] when the ledger is unavailable or the detail object is
/// rejected.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is one audited dimension: actor, action, resource, outcome, detail"
)]
pub async fn record(
    conn: &mut PgConnection,
    organization_id: Option<OrganizationId>,
    actor: Option<PrincipalId>,
    actor_kind: ActorKind,
    action: AuditAction,
    resource_type: &str,
    resource_id: Option<&str>,
    outcome: AuditOutcome,
    detail: Value,
) -> Result<(), AuditError> {
    audit::record(
        conn,
        &NewAuditEvent {
            organization_id,
            actor_principal_id: actor,
            actor_kind,
            action: action.as_str(),
            resource_type,
            resource_id,
            outcome,
            detail,
        },
    )
    .await?;
    Ok(())
}

/// Records a successful login and the new session.
///
/// # Errors
///
/// [`AuditError`] when the ledger is unavailable.
pub async fn login_succeeded(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    issuer: &str,
) -> Result<(), AuditError> {
    record(
        conn,
        Some(organization_id),
        Some(principal_id),
        ActorKind::User,
        AuditAction::AuthLoginSucceeded,
        "session",
        None,
        AuditOutcome::Allowed,
        json!({ "issuer": issuer }),
    )
    .await
}

/// Records a refused login. The reason is a stable code, never a token.
///
/// # Errors
///
/// [`AuditError`] when the ledger is unavailable.
pub async fn login_failed(
    conn: &mut PgConnection,
    organization_id: Option<OrganizationId>,
    issuer: Option<&str>,
    reason: &str,
) -> Result<(), AuditError> {
    record(
        conn,
        organization_id,
        None,
        ActorKind::Anonymous,
        AuditAction::AuthLoginFailed,
        "session",
        None,
        AuditOutcome::Denied,
        json!({ "reason": reason, "issuer": issuer }),
    )
    .await
}

/// Records an explicit logout.
///
/// # Errors
///
/// [`AuditError`] when the ledger is unavailable.
pub async fn logged_out(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
) -> Result<(), AuditError> {
    record(
        conn,
        Some(organization_id),
        Some(principal_id),
        ActorKind::User,
        AuditAction::AuthLoggedOut,
        "session",
        None,
        AuditOutcome::Allowed,
        json!({}),
    )
    .await
}

/// Records a rejected session or CSRF check.
///
/// # Errors
///
/// [`AuditError`] when the ledger is unavailable.
pub async fn session_denied(
    conn: &mut PgConnection,
    organization_id: Option<OrganizationId>,
    actor: Option<PrincipalId>,
    reason: &str,
) -> Result<(), AuditError> {
    // A denial raised after the session authenticated keeps the actor and its
    // kind (for example a CSRF failure); an unauthenticated denial is
    // anonymous.
    let kind = match (organization_id, actor) {
        (Some(organization_id), Some(actor)) => {
            actor_kind(conn, organization_id, Some(actor)).await
        }
        _ => ActorKind::Anonymous,
    };
    record(
        conn,
        organization_id,
        actor,
        kind,
        AuditAction::AuthSessionDenied,
        "session",
        None,
        AuditOutcome::Denied,
        json!({ "reason": reason }),
    )
    .await
}

/// Records a grant or revocation.
///
/// # Errors
///
/// [`AuditError`] when the ledger is unavailable.
pub async fn grant_changed(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: Option<PrincipalId>,
    action: AuditAction,
    principal_id: PrincipalId,
    role: &str,
    resource_id: Option<&str>,
) -> Result<(), AuditError> {
    let kind = actor_kind(conn, organization_id, actor).await;
    record(
        conn,
        Some(organization_id),
        actor,
        kind,
        action,
        "role_grant",
        resource_id,
        AuditOutcome::Allowed,
        json!({ "principal_id": principal_id.to_string(), "role": role }),
    )
    .await
}

/// Records a job decision made at admission or publication.
///
/// # Errors
///
/// [`AuditError`] when the ledger is unavailable.
pub async fn job_decision(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: Option<PrincipalId>,
    action: AuditAction,
    job_id: &str,
    outcome: AuditOutcome,
    reason: Option<&str>,
) -> Result<(), AuditError> {
    let kind = actor_kind(conn, organization_id, actor).await;
    record(
        conn,
        Some(organization_id),
        actor,
        kind,
        action,
        "job",
        Some(job_id),
        outcome,
        json!({ "reason": reason }),
    )
    .await
}

/// Records a newly provisioned principal.
///
/// # Errors
///
/// [`AuditError`] when the ledger is unavailable.
pub async fn principal_provisioned(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: Option<PrincipalId>,
    principal_id: PrincipalId,
    issuer: &str,
) -> Result<(), AuditError> {
    let kind = actor_kind(conn, organization_id, actor).await;
    record(
        conn,
        Some(organization_id),
        actor,
        kind,
        AuditAction::IdentityProvisioned,
        "principal",
        Some(&principal_id.to_string()),
        AuditOutcome::Allowed,
        json!({ "issuer": issuer }),
    )
    .await
}

/// Records a principal's deactivation or reactivation.
///
/// # Errors
///
/// [`AuditError`] when the ledger is unavailable.
pub async fn principal_state_changed(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: Option<PrincipalId>,
    action: AuditAction,
    principal: &Principal,
    sessions_revoked: u64,
) -> Result<(), AuditError> {
    let kind = actor_kind(conn, organization_id, actor).await;
    record(
        conn,
        Some(organization_id),
        actor,
        kind,
        action,
        "principal",
        Some(&principal.id.to_string()),
        AuditOutcome::Allowed,
        json!({
            "issuer": principal.issuer,
            "active": principal.is_active(),
            "sessions_revoked": sessions_revoked,
        }),
    )
    .await
}

/// Records an administrative session revocation.
///
/// # Errors
///
/// [`AuditError`] when the ledger is unavailable.
pub async fn sessions_revoked(
    conn: &mut PgConnection,
    organization_id: OrganizationId,
    actor: Option<PrincipalId>,
    principal_id: PrincipalId,
    revoked: u64,
) -> Result<(), AuditError> {
    let kind = actor_kind(conn, organization_id, actor).await;
    record(
        conn,
        Some(organization_id),
        actor,
        kind,
        AuditAction::SessionRevoked,
        "principal",
        Some(&principal_id.to_string()),
        AuditOutcome::Allowed,
        json!({ "sessions": revoked }),
    )
    .await
}
