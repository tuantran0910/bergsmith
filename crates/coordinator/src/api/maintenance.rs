//! Maintenance impact over the execution ledger (BS-027.T2).
//!
//! One table exposes its terminal jobs, newest first, each with the work its
//! accepted tasks rewrote, and totals over the requested range. The
//! organization reports the same totals over every table the caller may read
//! jobs on. Counts and byte totals are decimal strings so a large rewrite never
//! loses precision in a browser, and every timestamp is RFC 3339 in UTC. A
//! cursor carries the range start, so a later page keeps the range the first
//! page's totals describe.
//!
//! Work that a conflict discarded is counted, not hidden: the accepted output
//! of jobs that ended `ReplanRequired` is reported as `discarded_*`, which is
//! work done and then never published.

use std::str::FromStr;

use axum::Json;
use axum::extract::{Path, Query, State};
use bergsmith_common::ids::{JobId, TableId};
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::maintenance::{self, MaintenanceEntry, MaintenanceTotals};
use bergsmith_persistence::{grants, tables};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::pagination::{Page, PageQuery};
use super::statistics::instant;
use super::{ApiError, ApiState, Caller};

/// The cursor namespace for a table's maintenance history.
const TABLE_MAINTENANCE: &str = "table_maintenance";

/// How far back a range reaches when the request does not say.
const DEFAULT_RANGE_DAYS: i64 = 30;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MaintenanceQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    /// RFC 3339 lower bound, inclusive.
    since: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RangeQuery {
    /// RFC 3339 lower bound, inclusive.
    since: Option<String>,
}

/// The terminal jobs of `id`, newest first, with range totals.
pub(super) async fn table(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
    query: Result<Query<MaintenanceQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = TableId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let page = Page::parse(
        PageQuery::new(query.limit, query.cursor),
        caller.organization_id,
        caller.principal_id,
        TABLE_MAINTENANCE,
    )?;
    // The cursor carries the range start, so a page loaded later cannot move
    // the lower bound as "now" advances and silently drop jobs that the
    // first page's totals still include.
    let (since, after) = match page.after.as_deref() {
        Some(raw) => {
            let (since, after) = parse_after(raw)?;
            (since, Some(after))
        }
        None => (since(query.since.as_deref())?, None),
    };
    // An unknown or unreadable table is 404, as every table resource is; a
    // table the caller may see but not read jobs on is hidden the same way,
    // because this document is job data rather than inventory.
    let mut conn = state.database.pool().acquire().await?;
    let _ = tables::table_identity(&mut conn, caller.organization_id, id).await?;
    if !grants::has_permission(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        Permission::ViewJobs,
        ResourceScope::Table(id),
    )
    .await?
    {
        return Err(ApiError::not_found());
    }
    let limit = i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?;
    let entries =
        maintenance::history(&mut conn, caller.organization_id, id, since, after, limit).await?;
    let totals = maintenance::table_totals(&mut conn, caller.organization_id, id, since).await?;
    let items = entries
        .iter()
        .map(|entry| (encode_after(since, entry.job_id), entry_document(entry)))
        .collect();
    let mut body = page.finish(items)?;
    body["table_id"] = json!(id);
    body["since"] = json!(stamp(since));
    body["totals"] = totals_document(totals);
    Ok(Json(body))
}

/// Impact totals over every table the caller may read jobs on.
pub(super) async fn organization(
    State(state): State<ApiState>,
    caller: Caller,
    query: Result<Query<RangeQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let since = since(query.since.as_deref())?;
    let roles: Vec<String> = Permission::ViewJobs
        .roles()
        .iter()
        .map(|role| role.as_str().to_owned())
        .collect();
    let mut conn = state.database.pool().acquire().await?;
    let totals = maintenance::organization_totals(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        &roles,
        since,
    )
    .await?;
    Ok(Json(json!({
        "since": stamp(since),
        "totals": totals_document(totals),
    })))
}

/// The inclusive lower bound of a range, defaulting to thirty days ago.
fn since(raw: Option<&str>) -> Result<DateTime<Utc>, ApiError> {
    match raw {
        Some(value) => instant(value),
        None => Ok(Utc::now() - TimeDelta::days(DEFAULT_RANGE_DAYS)),
    }
}

/// The cursor key: the range start and the last job of the page.
///
/// [`Page`] treats the key as opaque and returns it as the next cursor, so the
/// range travels with the page without widening the shared cursor format. The
/// start keeps microsecond precision, the precision PostgreSQL stores for a
/// `timestamptz`: rounding it to milliseconds would widen the range on the
/// next page and pull in a job the first page excluded.
fn encode_after(since: DateTime<Utc>, job: JobId) -> String {
    format!("{}|{job}", since.timestamp_micros())
}

/// Reads a cursor key written by [`encode_after`].
fn parse_after(raw: &str) -> Result<(DateTime<Utc>, JobId), ApiError> {
    let (micros, job) = raw.split_once('|').ok_or_else(ApiError::invalid)?;
    let micros: i64 = micros.parse().map_err(|_| ApiError::invalid())?;
    let since = DateTime::from_timestamp_micros(micros).ok_or_else(ApiError::invalid)?;
    let job = JobId::from_str(job).map_err(|_| ApiError::invalid())?;
    Ok((since, job))
}

/// An instant as RFC 3339 with millisecond precision.
fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// A counter as a decimal string.
fn counter(value: i64) -> String {
    value.to_string()
}

/// The wire form of one terminal job.
fn entry_document(entry: &MaintenanceEntry) -> Value {
    json!({
        "job_id": entry.job_id,
        "state": entry.state.as_str(),
        "operation": entry.operation.api_name(),
        "created_at": stamp(entry.created_at),
        "updated_at": stamp(entry.updated_at),
        "elapsed_millis": counter(entry.elapsed_millis),
        "input_files": counter(entry.input_files),
        "input_bytes": counter(entry.input_bytes),
        "output_files": counter(entry.output_files),
        "output_bytes": counter(entry.output_bytes),
        "delete_files_applied": counter(entry.delete_files_applied),
    })
}

/// The wire form of impact totals.
fn totals_document(totals: MaintenanceTotals) -> Value {
    json!({
        "jobs": counter(totals.jobs),
        "succeeded": counter(totals.succeeded),
        "no_op": counter(totals.no_op),
        "cancelled": counter(totals.cancelled),
        "replanned": counter(totals.replanned),
        "failed": counter(totals.failed),
        "input_files": counter(totals.input_files),
        "input_bytes": counter(totals.input_bytes),
        "output_files": counter(totals.output_files),
        "output_bytes": counter(totals.output_bytes),
        "delete_files_applied": counter(totals.delete_files_applied),
        "discarded_files": counter(totals.discarded_files),
        "discarded_bytes": counter(totals.discarded_bytes),
        "abandoned_files": counter(totals.abandoned_files),
        "abandoned_bytes": counter(totals.abandoned_bytes),
    })
}
