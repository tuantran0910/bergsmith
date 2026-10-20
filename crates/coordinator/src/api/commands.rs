//! Job commands: cancellation and fresh-plan retry (BS-018.T2).
//!
//! Both commands are durable ledger operations, not execution shortcuts.
//! Cancellation follows the contract in §2: it reports whether it won, came
//! too late because publication was authorized, or found a terminal job.
//! Retry never revives a terminal job; it creates a new job with a fresh plan.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use bergsmith_common::audit::{ActorKind, AuditAction, AuditOutcome};
use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::JobId;
use bergsmith_common::job::JobState;
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::audit::NewAuditEvent;
use bergsmith_persistence::auth::Session;
use bergsmith_persistence::cancellation::{self, CancelJob, CancellationDisposition};
use bergsmith_persistence::idempotency::IdempotencyKey;
use bergsmith_persistence::{PersistenceError, audit, grants, jobs};
use serde::Deserialize;
use serde_json::{Value, json};

use super::creation::{self, JobCreation};
use super::{ApiError, ApiState, Caller, accepted_job, json_optional_body, resources};

/// The longest operator reason accepted for a cancellation.
const MAX_CANCEL_REASON_CHARS: usize = 500;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CancelJobRequest {
    reason: Option<String>,
}

/// Records a cancellation request and reports what it achieved.
///
/// Authorization, the cancellation, and its audit record commit together, so
/// a request the audit cannot record leaves the job unchanged. The request is
/// idempotent by contract: repeating it returns the same disposition and never
/// changes the first requester or reason.
pub(super) async fn cancel(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<uuid::Uuid>, axum::extract::rejection::PathRejection>,
    body: Result<Option<Json<CancelJobRequest>>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = JobId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    let request = json_optional_body(body)?.unwrap_or_default();
    let reason = normalize_reason(request.reason)?;

    let mut tx = state.database.pool().begin().await?;
    let job = jobs::job(&mut tx, caller.organization_id, id).await?;
    if let Err(error) = grants::require_permission(
        &mut tx,
        caller.organization_id,
        caller.principal_id,
        Permission::CancelJobs,
        ResourceScope::Table(job.table_id),
    )
    .await
    {
        tx.rollback().await?;
        if matches!(error, PersistenceError::PermissionDenied { .. }) {
            creation::record_denial_best_effort(
                &state,
                &caller,
                Permission::CancelJobs,
                job.table_id,
            )
            .await;
        }
        return Err(error.into());
    }
    let outcome = cancellation::request_cancellation(
        &mut tx,
        &CancelJob {
            job_id: id,
            requested_by: Some(caller.principal_id),
            reason,
        },
    )
    .await?;
    let disposition = disposition_name(outcome.disposition);
    // Record the request that was actually persisted, not the one that lost a
    // race: cancellation is monotonic, so a repeat keeps the first reason.
    audit::record(
        &mut tx,
        &NewAuditEvent {
            organization_id: Some(caller.organization_id),
            actor_principal_id: Some(caller.principal_id),
            actor_kind: ActorKind::User,
            action: AuditAction::JobCancellationRequested.as_str(),
            resource_type: "job",
            resource_id: Some(&id.to_string()),
            outcome: AuditOutcome::Allowed,
            detail: json!({
                "disposition": disposition,
                "reason": outcome
                    .job
                    .cancellation
                    .as_ref()
                    .and_then(|cancellation| cancellation.reason.clone()),
            }),
        },
    )
    .await?;
    tx.commit().await?;

    Ok(Json(json!({
        "job_id": outcome.job.id,
        "state": outcome.job.state,
        "revision": outcome.job.state_revision,
        "cancellation": disposition,
        "cancellation_requested": outcome.job.cancellation.is_some(),
        "publication_started": outcome.job.publication_started_at.is_some(),
    })))
}

/// Creates a fresh-plan retry of a terminal job.
///
/// The new job starts in `Planning` with `retry_of_job_id` naming the source.
/// No plan, output, or accepted result of the source job is reused.
pub(super) async fn retry(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<uuid::Uuid>, axum::extract::rejection::PathRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = JobId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    let key = retry_key(&caller.0, &headers, id)?;
    let mut conn = state.database.pool().acquire().await?;
    let source = jobs::job(&mut conn, caller.organization_id, id).await?;
    // Check before reporting the source state, so an unauthorized caller
    // cannot learn it; the mutation transaction checks again.
    resources::require(
        &mut conn,
        &caller.0,
        Permission::RetryJobs,
        ResourceScope::Table(source.table_id),
    )
    .await?;
    drop(conn);
    if !source.state.is_terminal() {
        return Err(ApiError::conflict(
            "job_not_terminal",
            "wait_for_job_outcome",
        ));
    }
    if source.state == JobState::Succeeded {
        return Err(ApiError::conflict("job_already_succeeded", "nothing_to_do"));
    }
    let retry = creation::create_job(
        &state,
        &caller,
        JobCreation {
            key,
            table_id: source.table_id,
            retry_of: Some(id),
            permission: Permission::RetryJobs,
            audit_action: AuditAction::JobRetryRequested,
            audit_detail: json!({
                "source_job_id": id,
                "source_state": source.state,
            }),
        },
    )
    .await?;
    Ok(accepted_job(retry, Some(id)))
}

/// Validates and normalizes an operator reason.
fn normalize_reason(reason: Option<String>) -> Result<Option<String>, ApiError> {
    let reason = reason
        .map(|reason| reason.trim().to_owned())
        .filter(|reason| !reason.is_empty());
    if reason.as_ref().is_some_and(|reason| {
        reason.chars().count() > MAX_CANCEL_REASON_CHARS || reason.chars().any(char::is_control)
    }) {
        return Err(ApiError::invalid());
    }
    Ok(reason)
}

/// The stable name of a cancellation outcome in responses and audit records.
const fn disposition_name(disposition: CancellationDisposition) -> &'static str {
    match disposition {
        CancellationDisposition::Cancelling => "cancelling",
        CancellationDisposition::TooLatePublicationStarted => "too_late_publication_started",
        CancellationDisposition::PublicationUnresolved => "publication_unresolved",
        CancellationDisposition::TooLateCommitted => "too_late_committed",
        CancellationDisposition::AlreadyEnded(JobState::Cancelled) => "cancelled",
        CancellationDisposition::AlreadyEnded(_) => "already_ended",
    }
}

/// Binds a retry request to its actor and source job.
fn retry_key(
    caller: &Session,
    headers: &HeaderMap,
    source: JobId,
) -> Result<IdempotencyKey, ApiError> {
    let key = headers
        .get("idempotency-key")
        .and_then(|key| key.to_str().ok())
        .filter(|key| {
            !key.is_empty() && key.len() <= 255 && key.bytes().all(|b| b.is_ascii_graphic())
        })
        .ok_or_else(ApiError::invalid)?;
    // The actor is part of the hash: a key cannot reveal another actor's job.
    let canonical =
        serde_json::to_vec(&(caller.principal_id, source)).map_err(|_| ApiError::invalid())?;
    Ok(IdempotencyKey {
        organization_id: caller.organization_id,
        key: key.into(),
        operation: "RetryJob".into(),
        request_hash: Sha256Digest::of(b"bergsmith.api.retry-job.v1", &canonical).to_hex(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_outcomes_have_stable_names() {
        for (disposition, name) in [
            (CancellationDisposition::Cancelling, "cancelling"),
            (
                CancellationDisposition::TooLatePublicationStarted,
                "too_late_publication_started",
            ),
            (
                CancellationDisposition::PublicationUnresolved,
                "publication_unresolved",
            ),
            (
                CancellationDisposition::TooLateCommitted,
                "too_late_committed",
            ),
            (
                CancellationDisposition::AlreadyEnded(JobState::Cancelled),
                "cancelled",
            ),
            (
                CancellationDisposition::AlreadyEnded(JobState::NoOp),
                "already_ended",
            ),
        ] {
            assert_eq!(disposition_name(disposition), name);
        }
    }

    #[test]
    fn cancellation_reasons_are_bounded_and_secret_free() {
        assert_eq!(normalize_reason(None).unwrap(), None);
        assert_eq!(
            normalize_reason(Some("  trim me  ".to_owned())).unwrap(),
            Some("trim me".to_owned())
        );
        assert_eq!(normalize_reason(Some("   ".to_owned())).unwrap(), None);
        assert!(normalize_reason(Some("x".repeat(MAX_CANCEL_REASON_CHARS + 1))).is_err());
        assert!(normalize_reason(Some("line\nbreak".to_owned())).is_err());
    }
}
