//! Durable job creation shared by submission and retry.
//!
//! One transaction authorizes the requester, binds the idempotency key,
//! creates the job and its `Created` event, and appends the audit record. A
//! failure after any part rolls the whole command back, so a job never exists
//! without its key or audit record.
//!
//! Authorization is rechecked inside the transaction: a grant revoked after
//! the request was routed still prevents the job.

use bergsmith_common::audit::AuditAction;
use bergsmith_common::ids::{JobId, TableId};
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::audit::NewAuditEvent;
use bergsmith_persistence::idempotency::{self, IdempotencyKey, IdempotencyStatus, RecordOutcome};
use bergsmith_persistence::jobs::{self, NewJob};
use bergsmith_persistence::{PersistenceError, audit, grants};
use serde_json::Value;

use super::resources::record_denial;
use super::{ApiError, ApiState, Caller};

/// The durable work one job-creating command performs.
pub(super) struct JobCreation {
    /// The idempotency key bound to the request and actor.
    pub(super) key: IdempotencyKey,
    /// The canonical table the job maintains.
    pub(super) table_id: TableId,
    /// The terminal job this one retries, if any.
    pub(super) retry_of: Option<JobId>,
    /// The permission the requester must hold on the table.
    pub(super) permission: Permission,
    /// The audit action recorded with the job.
    pub(super) audit_action: AuditAction,
    /// Secret-free audit detail.
    pub(super) audit_detail: Value,
}

/// Creates a job and returns its ID; a replay returns the original job.
///
/// # Errors
///
/// A typed [`ApiError`] for a denied or revoked grant, a reused key, a ledger
/// failure, or an audit failure. Nothing is committed in any of them.
pub(super) async fn create_job(
    state: &ApiState,
    caller: &Caller,
    creation: JobCreation,
) -> Result<JobId, ApiError> {
    let mut tx = state.database.pool().begin().await?;
    if let Err(error) = grants::require_permission(
        &mut tx,
        caller.organization_id,
        caller.principal_id,
        creation.permission,
        ResourceScope::Table(creation.table_id),
    )
    .await
    {
        tx.rollback().await?;
        if matches!(error, PersistenceError::PermissionDenied { .. }) {
            // Best effort, like every denial audit: the command is already
            // denied and an audit outage must not change the response.
            record_denial_best_effort(state, caller, creation.permission, creation.table_id).await;
        }
        return Err(error.into());
    }
    if let IdempotencyStatus::Replay { resource_id } =
        idempotency::check(&mut tx, &creation.key).await?
    {
        tx.rollback().await?;
        return Ok(JobId::from_uuid(resource_id));
    }
    let id = JobId::generate();
    jobs::create_job(
        &mut tx,
        &NewJob {
            id,
            organization_id: caller.organization_id,
            table_id: creation.table_id,
            retry_of: creation.retry_of,
            created_by: caller.principal_id,
        },
    )
    .await?;
    if let RecordOutcome::Replay { resource_id } =
        idempotency::record(&mut tx, &creation.key, *id.as_uuid()).await?
    {
        // A concurrent identical request won; discard this job and its event.
        tx.rollback().await?;
        return Ok(JobId::from_uuid(resource_id));
    }
    audit::record(
        &mut tx,
        &NewAuditEvent {
            organization_id: Some(caller.organization_id),
            actor_principal_id: Some(caller.principal_id),
            actor_kind: bergsmith_common::audit::ActorKind::User,
            action: creation.audit_action.as_str(),
            resource_type: "job",
            resource_id: Some(&id.to_string()),
            outcome: bergsmith_common::audit::AuditOutcome::Allowed,
            detail: creation.audit_detail,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Records a denial audit on its own connection, best effort.
///
/// The command is already denied; a pool or audit outage must not change the
/// response.
pub(super) async fn record_denial_best_effort(
    state: &ApiState,
    caller: &Caller,
    permission: Permission,
    table: TableId,
) {
    match state.database.pool().acquire().await {
        Ok(mut conn) => {
            record_denial(
                &mut conn,
                &caller.0,
                permission,
                ResourceScope::Table(table),
            )
            .await;
        }
        Err(error) => {
            tracing::warn!(error = %error, "api denial audit connection unavailable");
        }
    }
}
