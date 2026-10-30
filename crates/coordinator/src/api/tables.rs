//! The tables list, its health filter (BS-028.T4), and its name and catalog
//! filters and facets (BS-033.T4).
//!
//! A table's health is evaluated from the ledger when the page is read, never
//! stored (ADR 0059). The plain list appends the state of each row's table to
//! the page it already returns. Filtering or sorting by state needs the state
//! of every visible table, so that path walks the visible inventory in id
//! order, one bounded chunk at a time, and keeps the best `limit + 1` matches.
//! Its cursor carries the sort key, so a later page resumes after it. The
//! facets walk the same way, because health counts need every state.

use std::collections::{BTreeMap, BinaryHeap, HashMap};

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use bergsmith_common::ids::TableId;
use bergsmith_maintenance::HealthState;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::lists::{self, TableFilter};
use super::pagination::{Page, PageQuery};
use super::resources::{self, Resource};
use super::{ApiError, ApiState, Caller};
use crate::health::{self, Assessment};

/// How many tables one scan chunk evaluates.
///
/// A chunk costs one id read plus the three batched health reads, so this is
/// the memory bound of a filtered page, not its page size.
const SCAN_CHUNK: i64 = 250;

/// The worst state's rank; unassessed sorts last.
const UNASSESSED: u8 = 3;

/// The tables list's filters, beside the page bounds.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TablesQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    health: Option<HealthFilter>,
    sort: Option<TableSort>,
    catalog: Option<Uuid>,
    q: Option<String>,
}

impl TablesQuery {
    fn filter(&self) -> Result<TableFilter, ApiError> {
        Ok(TableFilter {
            catalog: self.catalog,
            q: lists::search(self.q.as_deref())?,
        })
    }
}

/// Every filter of a page, which its cursor scope digests.
#[derive(serde::Serialize)]
struct Scope<'a> {
    filter: &'a TableFilter,
    health: Option<&'static str>,
    sorted: bool,
}

/// An exact health state to keep.
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HealthFilter {
    Healthy,
    Warning,
    Critical,
    Unassessed,
}

impl HealthFilter {
    const fn rank(self) -> u8 {
        match self {
            Self::Critical => 0,
            Self::Warning => 1,
            Self::Healthy => 2,
            Self::Unassessed => UNASSESSED,
        }
    }

    /// The token a cursor repeats, so a filter cannot change mid-page.
    const fn token(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Warning => "warning",
            Self::Healthy => "healthy",
            Self::Unassessed => "unassessed",
        }
    }
}

/// The orders a health page supports.
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TableSort {
    Health,
}

/// `GET /api/v1/tables`, with optional name, catalog, and health filters and
/// the health order.
pub(super) async fn tables(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<TablesQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let filter = query.filter()?;
    let scope = Scope {
        filter: &filter,
        health: query.health.map(HealthFilter::token),
        sorted: query.sort.is_some(),
    };
    let filtered = filter != TableFilter::default() || scope.health.is_some() || scope.sorted;
    let page = Page::parse(
        PageQuery::new(query.limit, query.cursor),
        caller.organization_id,
        caller.principal_id,
        lists::scope(Resource::Table.name(), &scope, filtered)?,
    )?;
    if query.health.is_none() && query.sort.is_none() {
        return plain(&state, &caller, page, &filter).await;
    }
    scanned(&state, &caller, page, &filter, query.health).await
}

/// `GET /api/v1/tables/facets`: table counts by health and by catalog.
///
/// Each table counts once per catalog it has a visible alias in, so the
/// catalog counts can sum to more than the tables.
pub(super) async fn table_facets(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<TablesQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    if query.limit.is_some() || query.cursor.is_some() || query.sort.is_some() {
        return Err(ApiError::invalid());
    }
    let filter = query.filter()?;
    let wanted = query.health.map(HealthFilter::rank);
    // The catalog dimension ignores its own filter, so the scan applies only
    // the name filter and each dimension applies the other's in memory.
    let named = TableFilter {
        catalog: None,
        q: filter.q.clone(),
    };
    let now = Utc::now();
    let mut health = [0_i64; 4];
    let mut catalogs: BTreeMap<Uuid, i64> = BTreeMap::new();
    let mut after = None;
    loop {
        let tables = lists::visible_tables(&state, &caller.0, &named, after, SCAN_CHUNK).await?;
        let Some((last, _)) = tables.last() else {
            break;
        };
        after = Some(*last);
        let ids: Vec<Uuid> = tables.iter().map(|(id, _)| *id).collect();
        let assessments = assess(&state, &caller, &ids, now).await?;
        for (id, connections) in &tables {
            let Some(assessment) = assessments.get(&TableId::from_uuid(*id)) else {
                continue;
            };
            let rank = rank(assessment.state());
            if filter
                .catalog
                .is_none_or(|catalog| connections.contains(&catalog))
                && let Some(count) = health.get_mut(usize::from(rank))
            {
                *count += 1;
            }
            if wanted.is_none_or(|wanted| wanted == rank) {
                for connection in connections {
                    *catalogs.entry(*connection).or_default() += 1;
                }
            }
        }
        if tables.len() < usize::try_from(SCAN_CHUNK).map_err(|_| ApiError::internal())? {
            break;
        }
    }
    let health = [
        HealthFilter::Critical,
        HealthFilter::Warning,
        HealthFilter::Healthy,
        HealthFilter::Unassessed,
    ]
    .into_iter()
    .filter_map(|state| {
        let count = health.get(usize::from(state.rank())).copied().unwrap_or(0);
        (count > 0).then_some((state.token(), count))
    });
    Ok(Json(json!({
        "health": lists::facet(health),
        "catalog": lists::facet(catalogs.into_iter().map(|(id, count)| (id.to_string(), count))),
    })))
}

/// `GET /api/v1/tables/{id}`: the summary with its table's health.
pub(super) async fn table(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<axum::extract::Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = id.map_err(|_| ApiError::invalid())?.0;
    let Json(mut value) = resources::read(
        &state,
        &caller.0,
        Resource::Table,
        PageQuery::default(),
        Some(id),
    )
    .await?;
    let assessments = assess(&state, &caller, &[id], Utc::now()).await?;
    value["health"] = document(assessments.get(&TableId::from_uuid(id)));
    Ok(Json(value))
}

/// The plain list: the page's rows, each with its table's health.
async fn plain(
    state: &ApiState,
    caller: &Caller,
    page: Page,
    filter: &TableFilter,
) -> Result<Json<Value>, ApiError> {
    let after = page
        .after
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|_| ApiError::invalid())?;
    let limit = i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?;
    let mut rows = lists::table_page(state, &caller.0, filter, after, limit).await?;
    if !rows.is_empty() {
        let ids: Vec<Uuid> = rows.iter().map(|(id, _)| *id).collect();
        let assessments = assess(state, caller, &ids, Utc::now()).await?;
        for (id, value) in &mut rows {
            value["health"] = document(assessments.get(&TableId::from_uuid(*id)));
        }
    }
    Ok(Json(
        page.finish(
            rows.into_iter()
                .map(|(id, value)| (id.to_string(), value))
                .collect(),
        )?,
    ))
}

/// The health-ordered list: every visible table is evaluated, the matches are
/// kept worst first, and the page resumes after the cursor's sort key.
async fn scanned(
    state: &ApiState,
    caller: &Caller,
    page: Page,
    tables: &TableFilter,
    filter: Option<HealthFilter>,
) -> Result<Json<Value>, ApiError> {
    let cursor = health_cursor(&page, filter)?;
    let token = filter.map_or("any", HealthFilter::token);
    let wanted = filter.map(HealthFilter::rank);
    let capacity = page.limit + 1;
    let now = Utc::now();
    // The scan always starts at the beginning of the inventory: the order is
    // (state, id), so resuming from the cursor's id would skip matching tables
    // whose id sorts before it.
    let mut after = None;
    let mut best: BinaryHeap<(u8, Uuid)> = BinaryHeap::new();
    let mut selected: HashMap<Uuid, Assessment> = HashMap::new();
    loop {
        let ids: Vec<Uuid> = lists::visible_tables(state, &caller.0, tables, after, SCAN_CHUNK)
            .await?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        if ids.is_empty() {
            break;
        }
        let assessments = assess(state, caller, &ids, now).await?;
        for id in &ids {
            let Some(assessment) = assessments.get(&TableId::from_uuid(*id)) else {
                continue;
            };
            let rank = rank(assessment.state());
            if wanted.is_some_and(|wanted| wanted != rank) {
                continue;
            }
            let key = (rank, *id);
            if cursor.is_some_and(|cursor| key <= cursor) {
                continue;
            }
            keep(&mut best, &mut selected, key, assessment.clone(), capacity);
        }
        after = ids.last().copied();
        if ids.len() < usize::try_from(SCAN_CHUNK).map_err(|_| ApiError::internal())? {
            break;
        }
    }
    let keys = best.into_sorted_vec();
    let ids: Vec<Uuid> = keys.iter().map(|(_, id)| *id).collect();
    let mut summaries: HashMap<Uuid, Value> = resources::table_summaries(state, &caller.0, &ids)
        .await?
        .into_iter()
        .collect();
    let rows: Vec<(String, Value)> = keys
        .into_iter()
        .filter_map(|(rank, id)| {
            summaries.remove(&id).map(|mut value| {
                value["health"] = document(selected.get(&id));
                (format!("h:{token}:{rank}:{id}"), value)
            })
        })
        .collect();
    Ok(Json(page.finish(rows)?))
}

/// Keeps the `capacity` best keys, replacing the worst when `key` is better.
fn keep(
    best: &mut BinaryHeap<(u8, Uuid)>,
    selected: &mut HashMap<Uuid, Assessment>,
    key: (u8, Uuid),
    assessment: Assessment,
    capacity: usize,
) {
    if best.len() >= capacity {
        let Some(worst) = best.peek() else {
            return;
        };
        if key >= *worst {
            return;
        }
        if let Some(removed) = best.pop() {
            selected.remove(&removed.1);
        }
    }
    best.push(key);
    selected.insert(key.1, assessment);
}

/// The worst state's rank; a table without an assessment sorts last.
const fn rank(state: Option<HealthState>) -> u8 {
    match state {
        Some(HealthState::Critical) => 0,
        Some(HealthState::Warning) => 1,
        Some(HealthState::Healthy) => 2,
        None => UNASSESSED,
    }
}

/// The health-ordered cursor: `h:<filter>:<rank>:<table id>`, so a page
/// cannot be resumed under a different filter or order.
fn health_cursor(
    page: &Page,
    filter: Option<HealthFilter>,
) -> Result<Option<(u8, Uuid)>, ApiError> {
    page.after
        .as_deref()
        .map(|after| parse_health_cursor(after, filter.map_or("any", HealthFilter::token)))
        .transpose()
}

fn parse_health_cursor(after: &str, token: &str) -> Result<(u8, Uuid), ApiError> {
    let mut parts = after.splitn(4, ':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("h"), Some(found), Some(rank), Some(id)) => {
            if found != token {
                return Err(ApiError::invalid());
            }
            let rank: u8 = rank.parse().map_err(|_| ApiError::invalid())?;
            if rank > UNASSESSED {
                return Err(ApiError::invalid());
            }
            Ok((rank, Uuid::parse_str(id).map_err(|_| ApiError::invalid())?))
        }
        _ => Err(ApiError::invalid()),
    }
}

/// One table's health as the list carries it; `state` is null when unassessed.
fn document(assessment: Option<&Assessment>) -> Value {
    json!({
        "state": assessment.and_then(Assessment::state).map(HealthState::as_str),
        "sampled_at": assessment.and_then(|assessment| assessment.sampled_at).map(stamp),
        "confirmed_at": assessment.and_then(|assessment| assessment.confirmed_at).map(stamp),
    })
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// The health of each of `ids`, read from the ledger.
async fn assess(
    state: &ApiState,
    caller: &Caller,
    ids: &[Uuid],
    now: DateTime<Utc>,
) -> Result<HashMap<TableId, Assessment>, ApiError> {
    let tables: Vec<TableId> = ids.iter().map(|id| TableId::from_uuid(*id)).collect();
    let mut conn = state.database.pool().acquire().await?;
    Ok(health::assess(&mut conn, caller.organization_id, &tables, now).await?)
}
