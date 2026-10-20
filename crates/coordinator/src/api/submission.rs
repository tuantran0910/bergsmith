use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{JobId, TableId};
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::idempotency::{self, IdempotencyKey, IdempotencyStatus, RecordOutcome};
use bergsmith_persistence::jobs::{self, NewJob};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ApiError, ApiState, Caller, resources};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SubmitJob {
    table_id: TableId,
}

pub(super) async fn submit(
    State(state): State<ApiState>,
    caller: Caller,
    headers: HeaderMap,
    body: Result<Json<SubmitJob>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let request = body
        .map_err(|error| {
            if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request_too_large",
                    "reduce_request_size",
                )
            } else {
                ApiError::invalid()
            }
        })?
        .0;
    let key = submission_key(&caller.0, &headers, &request)?;
    let mut tx = state.database.pool().begin().await?;
    // Authorization is rechecked in the mutation transaction. A denial is
    // audited after rollback so the audit is not discarded with it.
    if let Err(error) = bergsmith_persistence::grants::require_permission(
        &mut tx,
        caller.organization_id,
        caller.principal_id,
        Permission::SubmitJobs,
        ResourceScope::Table(request.table_id),
    )
    .await
    {
        tx.rollback().await?;
        if matches!(
            error,
            bergsmith_persistence::PersistenceError::PermissionDenied { .. }
        ) {
            // Best effort, like every denial audit: the command is already
            // denied and an audit outage must not change the response.
            match state.database.pool().acquire().await {
                Ok(mut conn) => {
                    resources::record_denial(
                        &mut conn,
                        &caller.0,
                        Permission::SubmitJobs,
                        ResourceScope::Table(request.table_id),
                    )
                    .await;
                }
                Err(error) => {
                    tracing::warn!(error = %error, "api denial audit connection unavailable");
                }
            }
        }
        return Err(error.into());
    }
    if let IdempotencyStatus::Replay { resource_id } = idempotency::check(&mut tx, &key).await? {
        tx.rollback().await?;
        return Ok(accepted(JobId::from_uuid(resource_id)));
    }
    let id = JobId::generate();
    jobs::create_job(
        &mut tx,
        &NewJob {
            id,
            organization_id: caller.organization_id,
            table_id: request.table_id,
            retry_of: None,
            created_by: caller.principal_id,
        },
    )
    .await?;
    match idempotency::record(&mut tx, &key, *id.as_uuid()).await? {
        RecordOutcome::Replay { resource_id } => {
            // Roll back the losing job, its Created event, and any other work.
            tx.rollback().await?;
            return Ok(accepted(JobId::from_uuid(resource_id)));
        }
        RecordOutcome::Recorded => {}
    }
    bergsmith_persistence::audit::record(
        &mut tx,
        &bergsmith_persistence::audit::NewAuditEvent {
            organization_id: Some(caller.organization_id),
            actor_principal_id: Some(caller.principal_id),
            actor_kind: bergsmith_common::audit::ActorKind::User,
            action: bergsmith_common::audit::AuditAction::JobSubmitted.as_str(),
            resource_type: "job",
            resource_id: Some(&id.to_string()),
            outcome: bergsmith_common::audit::AuditOutcome::Allowed,
            detail: json!({"table_id": request.table_id}),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(accepted(id))
}

fn submission_key(
    caller: &bergsmith_persistence::auth::Session,
    headers: &HeaderMap,
    request: &SubmitJob,
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
        serde_json::to_vec(&(caller.principal_id, request)).map_err(|_| ApiError::invalid())?;
    Ok(IdempotencyKey {
        organization_id: caller.organization_id,
        key: key.into(),
        operation: "SubmitJob".into(),
        request_hash: Sha256Digest::of(b"bergsmith.api.submit-job.v1", &canonical).to_hex(),
    })
}

fn accepted(id: JobId) -> Response {
    let url = format!("/api/v1/jobs/{id}");
    (
        StatusCode::ACCEPTED,
        [(header::LOCATION, url.clone())],
        Json(json!({"operation_id": id, "job_id": id, "resource_url": url})),
    )
        .into_response()
}
