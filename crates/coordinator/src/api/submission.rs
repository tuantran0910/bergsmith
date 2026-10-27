use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use bergsmith_common::audit::AuditAction;
use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{PrincipalId, TableId};
use bergsmith_common::job::JobOperation;
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
    /// `compact` (the default), `expire_snapshots`, or
    /// `remove_orphan_files`.
    operation: Option<String>,
}

impl SubmitJob {
    /// The operation this request names, defaulting to compaction.
    fn job_operation(&self) -> Result<JobOperation, ApiError> {
        match self.operation.as_deref() {
            None | Some("compact") => Ok(JobOperation::Compact),
            Some("expire_snapshots") => Ok(JobOperation::ExpireSnapshots),
            Some("remove_orphan_files") => Ok(JobOperation::RemoveOrphanFiles),
            Some(_) => Err(ApiError::invalid()),
        }
    }
}

pub(super) async fn submit(
    State(state): State<ApiState>,
    caller: Caller,
    headers: HeaderMap,
    body: Result<Json<SubmitJob>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let request = json_body(body)?;
    let operation = request.job_operation()?;
    require_available(&state, operation)?;
    let key = submission_key(&caller.0, &headers, &request)?;
    let id = creation::create_job(
        &state,
        &caller,
        JobCreation {
            key,
            table_id: request.table_id,
            operation,
            retry_of: None,
            permission: Permission::SubmitJobs,
            audit_action: AuditAction::JobSubmitted,
            audit_detail: json!({
                "table_id": request.table_id,
                "operation": operation.as_str(),
            }),
        },
    )
    .await?;
    Ok(accepted_job(id, None))
}

/// The pre-operation request shape, used only for hashing.
///
/// A pre-upgrade submission had no `operation` field and must keep its hash so
/// a retry with the original key still replays the original job.
#[derive(Serialize)]
struct LegacySubmitJob {
    table_id: TableId,
}

fn canonical_submission(principal: PrincipalId, request: &SubmitJob) -> Result<Vec<u8>, ApiError> {
    if request.operation.is_none() {
        return serde_json::to_vec(&(
            principal,
            LegacySubmitJob {
                table_id: request.table_id,
            },
        ))
        .map_err(|_| ApiError::invalid());
    }
    serde_json::to_vec(&(principal, request)).map_err(|_| ApiError::invalid())
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
    let canonical = canonical_submission(caller.principal_id, request)?;
    Ok(IdempotencyKey {
        organization_id: caller.organization_id,
        key: key.into(),
        operation: "SubmitJob".into(),
        request_hash: Sha256Digest::of(b"bergsmith.api.submit-job.v1", &canonical).to_hex(),
    })
}

/// Refuses an operation this deployment has not enabled.
///
/// Submission and retry share the check, so a retry cannot start an
/// operation a fresh submission would be refused.
///
/// # Errors
///
/// 400 `expiration_ref_safety_unavailable` for snapshot expiration without
/// catalog reference safety, and 400 `orphan_cleanup_unavailable` for orphan
/// cleanup while `orphans.enabled` is off.
pub(super) fn require_available(state: &ApiState, operation: JobOperation) -> Result<(), ApiError> {
    match operation {
        JobOperation::ExpireSnapshots if !state.expiration_ref_safety => Err(ApiError::permanent(
            axum::http::StatusCode::BAD_REQUEST,
            "expiration_ref_safety_unavailable",
            "configure_a_catalog_with_atomic_reference_protection",
        )),
        JobOperation::RemoveOrphanFiles if !state.orphans_enabled => Err(ApiError::permanent(
            axum::http::StatusCode::BAD_REQUEST,
            "orphan_cleanup_unavailable",
            "enable_orphan_cleanup_in_the_deployment_configuration",
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_legacy_compaction_hash_is_preserved() {
        let principal = PrincipalId::generate();
        let table = TableId::generate();
        let request = SubmitJob {
            table_id: table,
            operation: None,
        };
        let legacy =
            serde_json::to_vec(&(principal, serde_json::json!({ "table_id": table }))).unwrap();
        assert_eq!(canonical_submission(principal, &request).unwrap(), legacy);

        let expiration = SubmitJob {
            table_id: table,
            operation: Some("expire_snapshots".to_owned()),
        };
        assert_ne!(
            canonical_submission(principal, &expiration).unwrap(),
            legacy
        );
    }
}
