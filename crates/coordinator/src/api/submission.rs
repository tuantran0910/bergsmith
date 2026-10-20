use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use bergsmith_common::audit::AuditAction;
use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::TableId;
use bergsmith_common::permission::Permission;
use bergsmith_persistence::auth::Session;
use bergsmith_persistence::idempotency::IdempotencyKey;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::creation::{self, JobCreation};
use super::{ApiError, ApiState, Caller, accepted_job, json_body};

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
    let request = json_body(body)?;
    let key = submission_key(&caller.0, &headers, &request)?;
    let id = creation::create_job(
        &state,
        &caller,
        JobCreation {
            key,
            table_id: request.table_id,
            retry_of: None,
            permission: Permission::SubmitJobs,
            audit_action: AuditAction::JobSubmitted,
            audit_detail: json!({"table_id": request.table_id}),
        },
    )
    .await?;
    Ok(accepted_job(id, None))
}

fn submission_key(
    caller: &Session,
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
