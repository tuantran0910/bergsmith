//! Filtered lists and facet counts (BS-033.T4, ADR 0067).
//!
//! The jobs, tables, catalogs, audit, and principals lists accept filters
//! beside their page bounds, and each has a `/facets` read that counts the
//! values of its facet dimensions under the same filters. A dimension's
//! counts ignore that dimension's own filter, so a person sees what widening
//! it would add. Each value is exactly the parameter value that selects it.
//!
//! Every query is assembled from compile-time fragments and binds each
//! request value. An absent filter adds no predicate, so PostgreSQL plans
//! each combination with a direct index condition instead of a nullable one.
//! Counts run through the list's grant predicate inside the query, so they
//! never reveal a resource the caller cannot list. A cursor is bound to a
//! digest of its filter, so a page cannot be resumed under another.

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bergsmith_common::audit::{AuditAction, AuditOutcome};
use bergsmith_common::ids::PrincipalId;
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_common::search::SearchText;
use bergsmith_persistence::audit::{self, AuditFilter};
use bergsmith_persistence::auth::Session;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgArguments;
use sqlx::{Arguments, Postgres, QueryBuilder};
use uuid::Uuid;

use super::pagination::{Page, PageQuery};
use super::{ApiError, ApiState, Caller, resources};

/// The most values one list filter accepts.
const MAX_VALUES: usize = 16;

/// The direction of an identifier-ordered list. Identifiers are `UUIDv7`, so
/// `newest` lists the most recently created first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Order {
    #[default]
    Oldest,
    Newest,
}

impl Order {
    /// The comparison that resumes after a cursor in this direction.
    const fn after(self) -> &'static str {
        match self {
            Self::Oldest => " AND r.id > ",
            Self::Newest => " AND r.id < ",
        }
    }

    const fn order_by(self) -> &'static str {
        match self {
            Self::Oldest => " ORDER BY r.id ASC LIMIT ",
            Self::Newest => " ORDER BY r.id DESC LIMIT ",
        }
    }
}

/// Parses a comma-separated filter of at most [`MAX_VALUES`] distinct values.
///
/// An absent parameter is no filter; an empty or unknown value is refused
/// rather than ignored, so a mistyped filter never widens the list.
pub(super) fn values<T: Ord>(
    raw: Option<&str>,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<BTreeSet<T>, ApiError> {
    let mut parsed = BTreeSet::new();
    let Some(raw) = raw else {
        return Ok(parsed);
    };
    for value in raw.split(',') {
        parsed.insert(parse(value).ok_or_else(ApiError::invalid)?);
        if parsed.len() > MAX_VALUES {
            return Err(ApiError::invalid());
        }
    }
    Ok(parsed)
}

/// Parses the `q` parameter.
pub(super) fn search(raw: Option<&str>) -> Result<Option<SearchText>, ApiError> {
    raw.map(SearchText::parse)
        .transpose()
        .map_err(|_| ApiError::invalid())
}

/// Parses a `true` or `false` filter.
pub(super) fn flag(raw: Option<&str>) -> Result<Option<bool>, ApiError> {
    match raw {
        None => Ok(None),
        Some("true") => Ok(Some(true)),
        Some("false") => Ok(Some(false)),
        Some(_) => Err(ApiError::invalid()),
    }
}

/// The cursor scope of `resource` under `filter`. An unfiltered list keeps
/// the bare resource name; a filtered one appends a digest of its canonical
/// form, so a cursor resumes only under the filter that produced it.
pub(super) fn scope(
    resource: &str,
    filter: &impl Serialize,
    filtered: bool,
) -> Result<String, ApiError> {
    if !filtered {
        return Ok(resource.to_owned());
    }
    let canonical = serde_json::to_vec(filter).map_err(|_| ApiError::internal())?;
    let digest = Sha256::digest(&canonical);
    Ok(format!(
        "{resource}:{}",
        URL_SAFE_NO_PAD.encode(&digest[..16])
    ))
}

/// One facet dimension: each value with its count, in the given order.
pub(super) fn facet<V: Serialize>(counts: impl IntoIterator<Item = (V, i64)>) -> Value {
    Value::Array(
        counts
            .into_iter()
            .map(|(value, count)| json!({ "value": value, "count": count }))
            .collect(),
    )
}

/// Refuses page bounds on a facet read, which has no pages.
fn no_pages(limit: Option<usize>, cursor: Option<&String>) -> Result<(), ApiError> {
    if limit.is_some() || cursor.is_some() {
        return Err(ApiError::invalid());
    }
    Ok(())
}

/// A query whose first three parameters are the caller's organization,
/// principal, and the roles that grant `permission`, as the `sql` access
/// fragments expect. Later binds are numbered from `$4`.
fn scoped(
    sql: &'static str,
    caller: &Session,
    permission: Permission,
) -> Result<QueryBuilder<Postgres>, ApiError> {
    let roles: Vec<&'static str> = permission
        .roles()
        .iter()
        .map(|role| role.as_str())
        .collect();
    let mut arguments = PgArguments::default();
    let bound = arguments
        .add(*caller.organization_id.as_uuid())
        .and_then(|()| arguments.add(*caller.principal_id.as_uuid()))
        .and_then(|()| arguments.add(roles));
    if bound.is_err() {
        tracing::error!("list query parameters could not be encoded");
        return Err(ApiError::internal());
    }
    Ok(QueryBuilder::with_arguments(sql, arguments))
}

/// Keeps rows of table `table` (a column) with a visible alias in `catalog`
/// and with a visible alias whose `namespace.name` contains `q`.
///
/// Only aliases the caller may see match: a table visible through one
/// connection must not be found by its name in another.
fn push_alias_filters(
    query: &mut QueryBuilder<Postgres>,
    table: &'static str,
    catalog: Option<Uuid>,
    q: Option<&SearchText>,
) {
    if let Some(catalog) = catalog {
        query
            .push(
                " AND EXISTS (SELECT 1 FROM table_aliases a \
                 WHERE a.organization_id = $1 AND a.table_identity_id = ",
            )
            .push(table)
            .push(" AND a.catalog_connection_id = ")
            .push_bind(catalog)
            .push(concat!(" AND ", alias_visible!("a"), ")"));
    }
    if let Some(q) = q {
        query
            .push(
                " AND EXISTS (SELECT 1 FROM table_aliases a \
                 WHERE a.organization_id = $1 AND a.table_identity_id = ",
            )
            .push(table)
            .push(" AND (array_to_string(a.namespace, '.') || '.' || a.table_name) ILIKE ")
            .push_bind(q.contains_pattern())
            .push(concat!(" ESCAPE '\\' AND ", alias_visible!("a"), ")"));
    }
}

// ---------------------------------------------------------------------------
// Jobs

/// `GET /api/v1/jobs` and `GET /api/v1/jobs/facets` parameters.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JobsQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    order: Option<Order>,
    state: Option<String>,
    operation: Option<String>,
    table: Option<Uuid>,
    catalog: Option<Uuid>,
    created_by: Option<Uuid>,
    q: Option<String>,
}

/// A validated jobs filter. States and operations hold persisted names.
#[derive(Default, PartialEq, Eq, Serialize)]
struct JobFilter {
    state: BTreeSet<&'static str>,
    operation: BTreeSet<&'static str>,
    table: Option<Uuid>,
    catalog: Option<Uuid>,
    created_by: Option<Uuid>,
    q: Option<SearchText>,
    order: Order,
}

/// A jobs facet dimension.
#[derive(Clone, Copy, PartialEq, Eq)]
enum JobFacet {
    State,
    Operation,
}

impl JobFilter {
    fn parse(query: &JobsQuery) -> Result<Self, ApiError> {
        Ok(Self {
            state: values(query.state.as_deref(), |name| {
                JobState::from_name(name).map(JobState::as_str)
            })?,
            operation: values(query.operation.as_deref(), |name| {
                JobOperation::from_api_name(name).map(JobOperation::as_str)
            })?,
            table: query.table,
            catalog: query.catalog,
            created_by: query.created_by,
            q: search(query.q.as_deref())?,
            order: query.order.unwrap_or_default(),
        })
    }

    /// Appends every filter except the one on `except`.
    fn push(&self, query: &mut QueryBuilder<Postgres>, except: Option<JobFacet>) {
        if except != Some(JobFacet::State) && !self.state.is_empty() {
            query
                .push(" AND r.state = ANY(")
                .push_bind(self.state.iter().copied().collect::<Vec<_>>())
                .push(")");
        }
        if except != Some(JobFacet::Operation) && !self.operation.is_empty() {
            query
                .push(" AND r.operation = ANY(")
                .push_bind(self.operation.iter().copied().collect::<Vec<_>>())
                .push(")");
        }
        if let Some(table) = self.table {
            query.push(" AND r.table_identity_id = ").push_bind(table);
        }
        if let Some(created_by) = self.created_by {
            query.push(" AND r.created_by = ").push_bind(created_by);
        }
        push_alias_filters(query, "r.table_identity_id", self.catalog, self.q.as_ref());
    }
}

macro_rules! jobs_from {
    () => {
        " FROM jobs r JOIN table_identities t \
         ON t.id = r.table_identity_id AND t.organization_id = r.organization_id \
         WHERE r.organization_id = $1 AND "
    };
}

/// `GET /api/v1/jobs`: the jobs the caller may view, filtered.
pub(super) async fn jobs(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<JobsQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let filter = JobFilter::parse(&query)?;
    let page = Page::parse(
        PageQuery::new(query.limit, query.cursor),
        caller.organization_id,
        caller.principal_id,
        scope("jobs", &filter, filter != JobFilter::default())?,
    )?;
    let after = page
        .after
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|_| ApiError::invalid())?;
    let mut sql = scoped(
        concat!(
            "SELECT r.id, ",
            job_summary!(),
            " AS document",
            jobs_from!(),
            table_access!()
        ),
        &caller.0,
        Permission::ViewJobs,
    )?;
    filter.push(&mut sql, None);
    if let Some(after) = after {
        sql.push(filter.order.after()).push_bind(after);
    }
    sql.push(filter.order.order_by())
        .push_bind(i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?);
    let rows: Vec<(Uuid, Value)> = sql
        .build_query_as()
        .fetch_all(state.database.pool())
        .await?;
    Ok(Json(
        page.finish(
            rows.into_iter()
                .map(|(id, value)| (id.to_string(), value))
                .collect(),
        )?,
    ))
}

/// `GET /api/v1/jobs/facets`: job counts by state and by operation.
pub(super) async fn job_facets(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<JobsQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    no_pages(query.limit, query.cursor.as_ref())?;
    if query.order.is_some() {
        return Err(ApiError::invalid());
    }
    let filter = JobFilter::parse(&query)?;

    let mut sql = scoped(
        concat!("SELECT r.state, count(*)", jobs_from!(), table_access!()),
        &caller.0,
        Permission::ViewJobs,
    )?;
    filter.push(&mut sql, Some(JobFacet::State));
    sql.push(" GROUP BY r.state");
    let mut states: Vec<(String, i64)> = sql
        .build_query_as()
        .fetch_all(state.database.pool())
        .await?;
    // Lifecycle order reads better than alphabetical.
    states.sort_by_key(|(name, _)| {
        JobState::ALL
            .iter()
            .position(|state| state.as_str() == name)
    });

    let mut sql = scoped(
        concat!(
            "SELECT r.operation, count(*)",
            jobs_from!(),
            table_access!()
        ),
        &caller.0,
        Permission::ViewJobs,
    )?;
    filter.push(&mut sql, Some(JobFacet::Operation));
    sql.push(" GROUP BY r.operation");
    let operations: Vec<(String, i64)> = sql
        .build_query_as()
        .fetch_all(state.database.pool())
        .await?;
    let operations = JobOperation::ALL.into_iter().filter_map(|operation| {
        operations
            .iter()
            .find(|(name, _)| name == operation.as_str())
            .map(|(_, count)| (operation.api_name(), *count))
    });

    Ok(Json(json!({
        "state": facet(states),
        "operation": facet(operations),
    })))
}

// ---------------------------------------------------------------------------
// Catalogs

/// `GET /api/v1/catalogs` and `GET /api/v1/catalogs/facets` parameters.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CatalogsQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    enabled: Option<String>,
    q: Option<String>,
}

#[derive(Default, PartialEq, Eq, Serialize)]
struct CatalogFilter {
    enabled: Option<bool>,
    q: Option<SearchText>,
}

impl CatalogFilter {
    fn parse(query: &CatalogsQuery) -> Result<Self, ApiError> {
        Ok(Self {
            enabled: flag(query.enabled.as_deref())?,
            q: search(query.q.as_deref())?,
        })
    }

    /// Appends the name filter and, unless counting it, the enabled filter.
    fn push(&self, query: &mut QueryBuilder<Postgres>, with_enabled: bool) {
        if let Some(enabled) = self.enabled.filter(|_| with_enabled) {
            query.push(" AND r.enabled = ").push_bind(enabled);
        }
        if let Some(q) = &self.q {
            query
                .push(" AND r.name ILIKE ")
                .push_bind(q.contains_pattern())
                .push(" ESCAPE '\\'");
        }
    }
}

/// `GET /api/v1/catalogs`: the catalog connections the caller may view.
pub(super) async fn catalogs(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<CatalogsQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let filter = CatalogFilter::parse(&query)?;
    let page = Page::parse(
        PageQuery::new(query.limit, query.cursor),
        caller.organization_id,
        caller.principal_id,
        scope("catalogs", &filter, filter != CatalogFilter::default())?,
    )?;
    let after = page
        .after
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|_| ApiError::invalid())?;
    let mut sql = scoped(
        concat!(
            "SELECT r.id, ",
            catalog_summary!(),
            " AS document FROM catalog_connections r WHERE r.organization_id = $1 AND ",
            catalog_access!()
        ),
        &caller.0,
        Permission::ViewInventory,
    )?;
    filter.push(&mut sql, true);
    if let Some(after) = after {
        sql.push(Order::Oldest.after()).push_bind(after);
    }
    sql.push(Order::Oldest.order_by())
        .push_bind(i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?);
    let rows: Vec<(Uuid, Value)> = sql
        .build_query_as()
        .fetch_all(state.database.pool())
        .await?;
    Ok(Json(
        page.finish(
            rows.into_iter()
                .map(|(id, value)| (id.to_string(), value))
                .collect(),
        )?,
    ))
}

/// `GET /api/v1/catalogs/facets`: catalog counts by whether they are enabled.
pub(super) async fn catalog_facets(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<CatalogsQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    no_pages(query.limit, query.cursor.as_ref())?;
    let filter = CatalogFilter::parse(&query)?;
    let mut sql = scoped(
        concat!(
            "SELECT r.enabled, count(*) FROM catalog_connections r WHERE r.organization_id = $1 AND ",
            catalog_access!()
        ),
        &caller.0,
        Permission::ViewInventory,
    )?;
    filter.push(&mut sql, false);
    // Enabled first.
    sql.push(" GROUP BY r.enabled ORDER BY r.enabled DESC");
    let enabled: Vec<(bool, i64)> = sql
        .build_query_as()
        .fetch_all(state.database.pool())
        .await?;
    Ok(Json(json!({
        "enabled": facet(enabled.into_iter().map(|(value, count)| (value.to_string(), count))),
    })))
}

// ---------------------------------------------------------------------------
// Tables

/// The tables list's name and catalog filters. Health is evaluated, not
/// stored, so the tables module applies it while scanning.
#[derive(Default, PartialEq, Eq, Serialize)]
pub(super) struct TableFilter {
    pub(super) catalog: Option<Uuid>,
    pub(super) q: Option<SearchText>,
}

macro_rules! tables_from {
    () => {
        " FROM table_identities r JOIN table_identities t ON t.id = r.id \
         WHERE r.organization_id = $1 AND "
    };
}

/// One page of the caller's tables matching `filter`, in id order.
pub(super) async fn table_page(
    state: &ApiState,
    caller: &Session,
    filter: &TableFilter,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<(Uuid, Value)>, ApiError> {
    let mut sql = scoped(
        concat!(
            "SELECT r.id, ",
            table_summary!(),
            " AS document",
            tables_from!(),
            table_access!()
        ),
        caller,
        Permission::ViewInventory,
    )?;
    push_alias_filters(&mut sql, "r.id", filter.catalog, filter.q.as_ref());
    if let Some(after) = after {
        sql.push(Order::Oldest.after()).push_bind(after);
    }
    sql.push(Order::Oldest.order_by()).push_bind(limit);
    Ok(sql
        .build_query_as()
        .fetch_all(state.database.pool())
        .await?)
}

/// The caller's tables matching `filter` strictly after `after`, ascending,
/// each with the connections of its visible aliases.
///
/// The health filter and facets walk the inventory in this order because a
/// table's state is evaluated from its sample rather than stored, so no
/// bounded SQL predicate can select a page of matches.
pub(super) async fn visible_tables(
    state: &ApiState,
    caller: &Session,
    filter: &TableFilter,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<(Uuid, Vec<Uuid>)>, ApiError> {
    let mut sql = scoped(
        concat!(
            "SELECT r.id, ARRAY(SELECT DISTINCT a.catalog_connection_id FROM table_aliases a \
             WHERE a.organization_id = $1 AND a.table_identity_id = r.id AND ",
            alias_visible!("a"),
            " ORDER BY 1)",
            tables_from!(),
            table_access!()
        ),
        caller,
        Permission::ViewInventory,
    )?;
    push_alias_filters(&mut sql, "r.id", filter.catalog, filter.q.as_ref());
    if let Some(after) = after {
        sql.push(Order::Oldest.after()).push_bind(after);
    }
    sql.push(Order::Oldest.order_by()).push_bind(limit);
    Ok(sql
        .build_query_as()
        .fetch_all(state.database.pool())
        .await?)
}

// ---------------------------------------------------------------------------
// Audit

/// `GET /api/v1/audit` and `GET /api/v1/audit/facets` parameters.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AuditQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    order: Option<Order>,
    outcome: Option<String>,
    action: Option<String>,
    resource_type: Option<String>,
    actor: Option<Uuid>,
    q: Option<String>,
}

/// The canonical form of an audit filter, which a cursor scope digests.
#[derive(Serialize)]
struct AuditScope<'a> {
    outcome: Vec<&'static str>,
    action: &'a BTreeSet<String>,
    resource_type: &'a BTreeSet<String>,
    actor: Option<PrincipalId>,
    q: Option<&'a SearchText>,
    order: Order,
}

/// Whether `value` can be a stored resource kind: lowercase words joined by
/// underscores, as every audit writer names them.
fn resource_type(value: &str) -> Option<String> {
    let valid = (1..=100).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_');
    valid.then(|| value.to_owned())
}

impl AuditQuery {
    fn filter(&self) -> Result<AuditFilter, ApiError> {
        Ok(AuditFilter {
            outcomes: values(self.outcome.as_deref(), AuditOutcome::from_name)?,
            actions: values(self.action.as_deref(), |name| {
                AuditAction::from_name(name).map(|action| action.as_str().to_owned())
            })?,
            resource_types: values(self.resource_type.as_deref(), resource_type)?,
            actor: self.actor.map(PrincipalId::from_uuid),
            actor_name: search(self.q.as_deref())?,
            newest_first: self.order == Some(Order::Newest),
        })
    }
}

/// `GET /api/v1/audit`: the organization's audit records, filtered.
pub(super) async fn audit(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let filter = query.filter()?;
    let scope_of = AuditScope {
        outcome: filter
            .outcomes
            .iter()
            .map(|outcome| outcome.as_str())
            .collect(),
        action: &filter.actions,
        resource_type: &filter.resource_types,
        actor: filter.actor,
        q: filter.actor_name.as_ref(),
        order: query.order.unwrap_or_default(),
    };
    let page = Page::parse(
        PageQuery::new(query.limit, query.cursor),
        caller.organization_id,
        caller.principal_id,
        scope("audit", &scope_of, filter != AuditFilter::default())?,
    )?;
    let after = page
        .after
        .as_deref()
        .map(str::parse::<i64>)
        .transpose()
        .map_err(|_| ApiError::invalid())?;
    if after.is_some_and(|id| id < 0) {
        return Err(ApiError::invalid());
    }
    let mut conn = state.database.pool().acquire().await?;
    resources::require(
        &mut conn,
        &caller.0,
        Permission::ViewAudit,
        ResourceScope::Organization,
    )
    .await?;
    let rows = audit::list_filtered(
        &mut conn,
        caller.organization_id,
        &filter,
        after,
        i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?,
    )
    .await?;
    let rows = rows.into_iter().map(|row| (row.id.to_string(), json!({
        // Encode bigint IDs as decimal strings so JavaScript preserves precision.
        "id": row.id.to_string(), "actor_principal_id": row.actor_principal_id,
        "actor_display_name": row.actor_display_name, "actor_kind": row.actor_kind.as_str(),
        "action": row.action,
        "resource_type": row.resource_type, "resource_id": row.resource_id,
        "outcome": row.outcome.as_str(), "detail": row.detail, "occurred_at": row.occurred_at,
    }))).collect();
    Ok(Json(page.finish(rows)?))
}

/// `GET /api/v1/audit/facets`: audit record counts by outcome, action, and
/// resource kind.
pub(super) async fn audit_facets(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    no_pages(query.limit, query.cursor.as_ref())?;
    if query.order.is_some() {
        return Err(ApiError::invalid());
    }
    let filter = query.filter()?;
    let mut conn = state.database.pool().acquire().await?;
    resources::require(
        &mut conn,
        &caller.0,
        Permission::ViewAudit,
        ResourceScope::Organization,
    )
    .await?;
    let facets = audit::facets(&mut conn, caller.organization_id, &filter).await?;
    Ok(Json(json!({
        "outcome": facet(facets.outcome.into_iter().map(|(outcome, count)| (outcome.as_str(), count))),
        "action": facet(facets.action),
        "resource_type": facet(facets.resource_type),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_list_is_distinct_and_bounded() {
        let parsed = values(Some("b,a,b"), |value| Some(value.to_owned())).unwrap();
        assert_eq!(parsed.into_iter().collect::<Vec<_>>(), ["a", "b"]);
        let many = (0..=MAX_VALUES)
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
            .join(",");
        assert!(values(Some(&many), |value| Some(value.to_owned())).is_err());
    }

    #[test]
    fn an_empty_or_unknown_value_is_refused() {
        let known = |value: &str| (value == "a").then(|| value.to_owned());
        assert!(values(Some(""), known).is_err());
        assert!(values(Some("a,"), known).is_err());
        assert!(values(Some("a,z"), known).is_err());
        assert!(values(None, known).unwrap().is_empty());
    }

    #[test]
    fn flags_are_exact() {
        assert_eq!(flag(Some("true")).unwrap(), Some(true));
        assert_eq!(flag(Some("false")).unwrap(), Some(false));
        assert_eq!(flag(None).unwrap(), None);
        assert!(flag(Some("1")).is_err());
        assert!(flag(Some("TRUE")).is_err());
    }

    #[test]
    fn a_cursor_scope_names_its_filter() {
        let empty = CatalogFilter::default();
        assert_eq!(scope("catalogs", &empty, false).unwrap(), "catalogs");
        let one = CatalogFilter {
            enabled: Some(true),
            q: None,
        };
        let other = CatalogFilter {
            enabled: Some(false),
            q: None,
        };
        let first = scope("catalogs", &one, true).unwrap();
        assert!(first.starts_with("catalogs:"));
        assert_eq!(first, scope("catalogs", &one, true).unwrap());
        assert_ne!(first, scope("catalogs", &other, true).unwrap());
    }

    #[test]
    fn facets_carry_values_and_counts_in_order() {
        assert_eq!(
            facet([("Running", 2), ("Failed", 1)]),
            json!([
                {"value": "Running", "count": 2},
                {"value": "Failed", "count": 1},
            ])
        );
    }
}
