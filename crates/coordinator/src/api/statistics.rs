//! Table statistics over the metadata-only samples (BS-025.T3).
//!
//! One table exposes its newest sample and a bounded history. Catalogs and the
//! organization aggregate the newest sample of every table the caller may see.
//! Counts and byte totals are decimal strings so a large table never loses
//! precision in a browser; every timestamp is RFC 3339 in UTC.

use axum::Json;
use axum::extract::{Path, Query, State};
use bergsmith_common::ids::{CatalogConnectionId, TableId};
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_maintenance::{PartitionFiles, SizeBucket};
use bergsmith_persistence::grants;
use bergsmith_persistence::statistics::{self, StatisticsTotals, StoredTableSample};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::pagination::{Page, PageQuery};
use super::resources::{self, Resource};
use super::{ApiError, ApiState, Caller};

/// The cursor namespace for a table's sample history.
const TABLE_STATISTICS: &str = "table_statistics";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StatisticsQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    /// RFC 3339 lower bound, inclusive.
    since: Option<String>,
}

/// The newest sample of `id` plus its history, newest first.
pub(super) async fn table(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
    query: Result<Query<StatisticsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = TableId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let page = Page::parse(
        PageQuery::new(query.limit, query.cursor),
        caller.organization_id,
        caller.principal_id,
        TABLE_STATISTICS,
    )?;
    let since = query.since.as_deref().map(instant).transpose()?;
    let before = page.after.as_deref().map(instant).transpose()?;
    // The table's own resource decides visibility, so an unknown, ungranted,
    // or deleted table is 404 here too.
    let _ = resources::read(
        &state,
        &caller.0,
        Resource::Table,
        PageQuery::default(),
        Some(*id.as_uuid()),
    )
    .await?;
    let limit = i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?;
    let mut conn = state.database.pool().acquire().await?;
    // Partition values are table metadata that only a principal allowed to
    // plan may see; planning previews expose the same values.
    let with_partitions = grants::has_permission(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        Permission::SubmitJobs,
        ResourceScope::Table(id),
    )
    .await?;
    let latest = statistics::latest(&mut conn, caller.organization_id, id).await?;
    let samples =
        statistics::history(&mut conn, caller.organization_id, id, before, since, limit).await?;
    let now = Utc::now();
    let mut items = Vec::with_capacity(samples.len());
    for sample in samples {
        items.push((
            key(sample.sampled_at),
            document(&sample, now, with_partitions)?,
        ));
    }
    let mut body = page.finish(items)?;
    body["latest"] = match latest {
        Some(sample) => document(&sample, now, with_partitions)?,
        None => Value::Null,
    };
    Ok(Json(body))
}

/// Totals over the visible tables of one catalog connection.
pub(super) async fn catalog(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let connection = CatalogConnectionId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    // The connection resource decides visibility, so an inaccessible
    // connection is 404 rather than an empty total.
    let _ = resources::read(
        &state,
        &caller.0,
        Resource::Catalog,
        PageQuery::default(),
        Some(*connection.as_uuid()),
    )
    .await?;
    let totals = totals(&state, &caller.0, Some(connection)).await?;
    Ok(Json(totals_document(totals)))
}

/// Totals over every table the caller may see in the organization.
pub(super) async fn organization(
    State(state): State<ApiState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    let totals = totals(&state, &caller.0, None).await?;
    Ok(Json(totals_document(totals)))
}

async fn totals(
    state: &ApiState,
    caller: &bergsmith_persistence::auth::Session,
    connection: Option<CatalogConnectionId>,
) -> Result<StatisticsTotals, ApiError> {
    let roles: Vec<String> = Permission::ViewInventory
        .roles()
        .iter()
        .map(|role| role.as_str().to_owned())
        .collect();
    let mut conn = state.database.pool().acquire().await?;
    Ok(statistics::totals(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        &roles,
        connection,
    )
    .await?)
}

/// The cursor key of a sample: its instant at the ledger's precision.
fn key(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn instant(raw: &str) -> Result<DateTime<Utc>, ApiError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|at| at.with_timezone(&Utc))
        .map_err(|_| ApiError::invalid())
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn counter(value: i64) -> String {
    value.to_string()
}

/// Whole seconds since `at`, never negative.
fn age_seconds(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    at.map(|at| (now - at).num_seconds().max(0).to_string())
}

/// The API document of one stored sample.
///
/// `with_partitions` decides whether partition values are included; the
/// partition count stays visible either way.
fn document(
    sample: &StoredTableSample,
    now: DateTime<Utc>,
    with_partitions: bool,
) -> Result<Value, ApiError> {
    let histogram: Vec<SizeBucket> = serde_json::from_value(sample.size_histogram.clone())
        .map_err(|error| {
            tracing::error!(error = %error, "stored size histogram is unreadable");
            ApiError::internal()
        })?;
    let partitions: Vec<PartitionFiles> = if with_partitions {
        serde_json::from_value(sample.top_partitions.clone()).map_err(|error| {
            tracing::error!(error = %error, "stored partition summary is unreadable");
            ApiError::internal()
        })?
    } else {
        Vec::new()
    };
    Ok(json!({
        "sampled_at": stamp(sample.sampled_at),
        "catalog_connection_id": sample.catalog_connection_id.as_uuid(),
        "snapshot_id": sample.snapshot_id.map(|id| id.to_string()),
        "last_operation": sample.last_operation,
        "record_count": counter(sample.record_count),
        "data_files": counter(sample.data_files),
        "data_bytes": counter(sample.data_bytes),
        "delete_files": counter(sample.delete_files),
        "delete_bytes": counter(sample.delete_bytes),
        "position_delete_files": counter(sample.position_delete_files),
        "equality_delete_files": counter(sample.equality_delete_files),
        "position_delete_records": counter(sample.position_delete_records),
        "equality_delete_records": counter(sample.equality_delete_records),
        "manifest_count": counter(sample.manifest_count),
        "manifest_bytes": counter(sample.manifest_bytes),
        "partition_count": counter(sample.partition_count),
        "small_files": counter(sample.small_files),
        "target_file_size_bytes": counter(sample.target_file_size_bytes),
        "snapshot_count": counter(sample.snapshot_count),
        "oldest_snapshot_at": sample.oldest_snapshot_at.map(stamp),
        "newest_snapshot_at": sample.newest_snapshot_at.map(stamp),
        "oldest_snapshot_age_seconds": age_seconds(sample.oldest_snapshot_at, now),
        "newest_snapshot_age_seconds": age_seconds(sample.newest_snapshot_at, now),
        "size_histogram": histogram
            .iter()
            .map(|bucket| json!({
                "lower_bound_bytes": bucket.lower_bound_bytes.to_string(),
                "upper_bound_bytes": bucket.upper_bound_bytes.map(|bound| bound.to_string()),
                "files": bucket.files.to_string(),
                "bytes": bucket.bytes.to_string(),
            }))
            .collect::<Vec<_>>(),
        "top_partitions": partitions
            .iter()
            .map(|partition| json!({
                "spec_id": partition.spec_id,
                "partition": partition.partition,
                "files": partition.files.to_string(),
                "bytes": partition.bytes.to_string(),
            }))
            .collect::<Vec<_>>(),
    }))
}

/// The API document of an aggregate.
fn totals_document(totals: StatisticsTotals) -> Value {
    json!({
        "tables": counter(totals.tables),
        "record_count": counter(totals.record_count),
        "data_files": counter(totals.data_files),
        "data_bytes": counter(totals.data_bytes),
        "delete_files": counter(totals.delete_files),
        "delete_bytes": counter(totals.delete_bytes),
        "small_files": counter(totals.small_files),
        "snapshot_count": counter(totals.snapshot_count),
        "last_sampled_at": totals.last_sampled_at.map(stamp),
    })
}
