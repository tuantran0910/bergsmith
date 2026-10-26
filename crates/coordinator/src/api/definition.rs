//! The current definition of one table (BS-026.T3).
//!
//! A definition is metadata-only: the coordinator loads the table from its
//! catalog and reports the current schema, every partition spec, the default
//! sort order, and an allow-listed set of table properties. No data or delete
//! file is opened, no partition value is included, and the response carries no
//! metadata location, credential, or catalog connection setting.

use axum::Json;
use axum::extract::State;
use bergsmith_common::ids::TableId;
use bergsmith_common::permission::Permission;
use serde_json::{Value, json};

use super::preview::{MissingTable, failed, resolve};
use super::{ApiError, ApiState, Caller};

/// Reports `table`'s definition from its catalog metadata.
///
/// Reading a definition is an inventory read: a caller allowed to view the
/// table sees its schema, specs, and sort order, and the property allow-list
/// never widens to whatever the table happens to set.
pub(super) async fn definition(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<axum::extract::Path<uuid::Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let table = TableId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    // An inventory read hides existence: a table outside the caller's grants
    // is 404, exactly as `GET /api/v1/tables/{id}` reports it.
    let target = resolve(
        &state,
        &caller,
        table,
        Permission::ViewInventory,
        MissingTable::NotFound,
    )
    .await?;
    let definition = state
        .previews
        .definition(&target)
        .await
        .map_err(|error| failed(table, &error))?;
    let mut document = serde_json::to_value(definition).map_err(|error| {
        tracing::error!(error = %error, "table definition does not serialize");
        ApiError::internal()
    })?;
    document["table_id"] = json!(table);
    Ok(Json(document))
}
