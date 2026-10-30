//! Name search across the inventory (BS-033.T7, ADR 0067).
//!
//! `GET /api/v1/search?q=` answers the command palette with a few catalogs,
//! tables, and people whose display names contain `q`. Each kind is read
//! through its list's grant predicate inside the query, so a result never
//! names a resource the caller cannot list, and a table is found only by an
//! alias the caller may see. People are searched only for callers who may
//! manage access; otherwise that section is `null`. Like the lists, `q` is
//! capped and its `LIKE` wildcards are escaped; rows are never searched.

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::grants;
use bergsmith_persistence::tenancy::{self, PrincipalFilter};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::lists::{scoped, search as search_text};
use super::{ApiError, ApiState, Caller};

/// Results per kind when `limit` is absent.
const DEFAULT_LIMIT: usize = 5;

/// The most results per kind.
const MAX_LIMIT: usize = 20;

/// `GET /api/v1/search` parameters.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SearchQuery {
    q: Option<String>,
    limit: Option<usize>,
}

/// `GET /api/v1/search`: catalogs, tables, and people matching `q`.
pub(super) async fn search(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<SearchQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let q = search_text(query.q.as_deref())?.ok_or_else(ApiError::invalid)?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::invalid());
    }
    let bound = i64::try_from(limit).map_err(|_| ApiError::invalid())?;
    let pattern = q.contains_pattern();

    let mut sql = scoped(
        concat!(
            "SELECT r.id, r.name, r.enabled FROM catalog_connections r \
             WHERE r.organization_id = $1 AND ",
            catalog_access!()
        ),
        &caller.0,
        Permission::ViewInventory,
    )?;
    sql.push(" AND r.name ILIKE ")
        .push_bind(&pattern)
        .push(" ESCAPE '\\' ORDER BY lower(r.name), r.id LIMIT ")
        .push_bind(bound);
    let catalogs: Vec<(Uuid, String, bool)> = sql
        .build_query_as()
        .fetch_all(state.database.pool())
        .await?;

    let mut sql = scoped(
        concat!(
            "SELECT a.table_identity_id, a.catalog_connection_id, a.namespace, a.table_name \
             FROM table_aliases a JOIN table_identities t \
               ON t.id = a.table_identity_id AND t.organization_id = a.organization_id \
             WHERE a.organization_id = $1 AND ",
            table_access!(),
            " AND ",
            alias_visible!("a")
        ),
        &caller.0,
        Permission::ViewInventory,
    )?;
    sql.push(" AND (array_to_string(a.namespace, '.') || '.' || a.table_name) ILIKE ")
        .push_bind(&pattern)
        .push(
            " ESCAPE '\\' ORDER BY lower(a.table_name), a.namespace, \
             a.catalog_connection_id LIMIT ",
        )
        .push_bind(bound);
    let tables: Vec<(Uuid, Uuid, Vec<String>, String)> = sql
        .build_query_as()
        .fetch_all(state.database.pool())
        .await?;

    let mut conn = state.database.pool().acquire().await?;
    let principals = if grants::has_permission(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        Permission::ManageGrants,
        ResourceScope::Organization,
    )
    .await?
    {
        let filter = PrincipalFilter {
            kind: None,
            active: None,
            display_name: Some(q),
        };
        Some(
            tenancy::list_principals(&mut conn, caller.organization_id, &filter, None, limit)
                .await?,
        )
    } else {
        None
    };

    Ok(Json(json!({
        "catalogs": catalogs
            .into_iter()
            .map(|(id, name, enabled)| json!({ "id": id, "name": name, "enabled": enabled }))
            .collect::<Vec<_>>(),
        "tables": tables
            .into_iter()
            .map(|(id, catalog_id, namespace, name)| json!({
                "id": id,
                "catalog_id": catalog_id,
                "namespace": namespace,
                "name": name,
            }))
            .collect::<Vec<_>>(),
        "principals": principals.map(|principals| principals
            .iter()
            .map(|principal| json!({
                "id": principal.id,
                "kind": principal.kind.as_str(),
                "display_name": principal.display_name,
                "active": principal.is_active(),
            }))
            .collect::<Vec<_>>()),
    })))
}
