//! Ledger-only, inventory-authorized storage reads.
use super::{
    ApiError, ApiState, Caller,
    pagination::PageQuery,
    resources::{self, Resource},
};
use axum::{
    Json,
    extract::{Path, State},
};
use bergsmith_common::{
    ids::{CatalogConnectionId, TableId},
    permission::Permission,
};
use bergsmith_persistence::storage_breakdown;
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

pub(super) async fn table(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = id.map_err(|_| ApiError::invalid())?.0;
    let _ = resources::read(
        &state,
        &caller.0,
        Resource::Table,
        PageQuery::default(),
        Some(id),
    )
    .await?;
    let mut conn = state.database.pool().acquire().await?;
    let stored =
        storage_breakdown::latest(&mut conn, caller.organization_id, TableId::from_uuid(id))
            .await?;
    let mut body = json!({"enabled":state.storage_breakdown.enabled,"latest":null,"checked_at":null,"failure_reason":null});
    if let Some(stored) = stored {
        body["checked_at"] = json!(stored.checked_at);
        body["failure_reason"] = json!(stored.failure_reason);
        if let (Some(mut content), Some(at)) = (stored.content, stored.sampled_at) {
            for bucket in [
                "live",
                "retained",
                "metadata",
                "orphan",
                "protected",
                "total",
                "external",
            ] {
                for counter in ["files", "bytes"] {
                    let count = content[bucket][counter]
                        .as_u64()
                        .ok_or_else(ApiError::internal)?;
                    content[bucket][counter] = json!(count.to_string());
                }
            }
            content["sampled_at"] = json!(at);
            content["snapshot_id"] = json!(stored.snapshot_id.map(|id| id.to_string()));
            content["stale"] = json!(
                at < state.storage_breakdown.fresh_after(Utc::now())
                    || stored.failure_reason.is_some()
            );
            // Millisecond cutoff is an instant, not a browser numeric counter.
            content["cutoff_at"] = json!(
                content["cutoff_ms"]
                    .as_i64()
                    .and_then(chrono::DateTime::from_timestamp_millis)
            );
            content
                .as_object_mut()
                .ok_or_else(ApiError::internal)?
                .remove("cutoff_ms");
            body["latest"] = content;
        }
    }
    Ok(Json(body))
}

pub(super) async fn catalog(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = id.map_err(|_| ApiError::invalid())?.0;
    let _ = resources::read(
        &state,
        &caller.0,
        Resource::Catalog,
        PageQuery::default(),
        Some(id),
    )
    .await?;
    let roles: Vec<String> = Permission::ViewInventory
        .roles()
        .iter()
        .map(|role| role.as_str().to_owned())
        .collect();
    let mut conn = state.database.pool().acquire().await?;
    let mut body = storage_breakdown::totals(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        &roles,
        CatalogConnectionId::from_uuid(id),
        state.storage_breakdown.fresh_after(Utc::now()),
    )
    .await?;
    body["enabled"] = json!(state.storage_breakdown.enabled);
    Ok(Json(body))
}
