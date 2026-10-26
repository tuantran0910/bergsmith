//! Table health findings and their recommendations (BS-028.T2).
//!
//! The newest metadata-only sample is evaluated with ADR 0059's model. Each
//! finding carries the action that resolves it. Actions the planner can run
//! today get the prefilled preview that would assess them and its estimate;
//! the rest say why they cannot run yet. The response is derived from the
//! ledger, except the estimate, which loads the table through the same preview
//! path a submission uses.
//!
//! Counters cross the API as decimal strings, as every other counter does.

use axum::Json;
use axum::extract::{Path, State};
use bergsmith_common::health::HealthThresholds;
use bergsmith_common::ids::TableId;
use bergsmith_common::permission::Permission;
use bergsmith_maintenance::{
    Estimate, Finding, Recommendation, TableHealth, TableSample, ThresholdSource, evaluate,
    recommend,
};
use bergsmith_persistence::statistics::{self, StoredTableSample};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::preview::AssessedPreview;

use super::pagination::PageQuery;
use super::preview::{MissingTable, failed, outcome, resolve};
use super::resources::{self, Resource};
use super::{ApiError, ApiState, Caller};

/// The health of `id` and the recommended action for each finding.
///
/// Requires `jobs:submit`, because an actionable recommendation carries a
/// catalog assessment: the caller learns what a job would touch. A table
/// without a sample has no state and no recommendation, and the response is
/// served without a catalog request.
pub(super) async fn table(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = TableId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    // Visibility comes first: a table outside the caller's inventory reads as
    // unknown, so a missing grant cannot confirm that the table exists. Only
    // a visible table without `jobs:submit` is a 403.
    let _ = resources::read(
        &state,
        &caller.0,
        Resource::Table,
        PageQuery::default(),
        Some(*id.as_uuid()),
    )
    .await?;
    let target = resolve(
        &state,
        &caller,
        id,
        Permission::SubmitJobs,
        MissingTable::Forbidden,
    )
    .await?;
    let (latest, confirmed) = {
        let mut conn = state.database.pool().acquire().await?;
        let latest = statistics::latest(&mut conn, caller.organization_id, id).await?;
        let confirmed =
            statistics::last_confirmed_at(&mut conn, caller.organization_id, id).await?;
        (latest, confirmed)
    };
    let Some(stored) = latest else {
        return Ok(Json(unassessed(id, None, None)));
    };
    // A sample written before the health counters existed carries zeros the
    // model cannot tell from measurements, so it is not assessed; the
    // collector replaces it with a complete observation on its next pass.
    if stored.sample_version < statistics::SAMPLE_VERSION {
        return Ok(Json(unassessed(
            id,
            Some(stored.sampled_at),
            Some(confirmed.unwrap_or(stored.sampled_at)),
        )));
    }
    let sample = sample_model(&stored)?;
    let confirmed_at = confirmed.unwrap_or(stored.sampled_at);
    let health = evaluate(
        &sample,
        confirmed_at,
        &HealthThresholds::default(),
        ThresholdSource::Defaults,
        Utc::now(),
    )
    .map_err(|_| ApiError::internal())?;
    let recommendations: Vec<Recommendation> = health
        .findings
        .iter()
        .map(|finding| recommend(finding.kind))
        .collect();
    let assessed = if recommendations
        .iter()
        .any(|recommendation| recommendation.advisory.is_none())
    {
        Some(
            state
                .previews
                .preview_target(caller.principal_id, &target)
                .await
                .map_err(|error| failed(id, &error))?,
        )
    } else {
        None
    };
    Ok(Json(document(
        id,
        &stored,
        confirmed_at,
        &health,
        &recommendations,
        assessed.as_ref(),
    )))
}

/// A table with no health: unknown, never sampled, or sampled before the
/// current counter version. The instants stay visible when a sample exists.
fn unassessed(
    id: TableId,
    sampled_at: Option<DateTime<Utc>>,
    confirmed_at: Option<DateTime<Utc>>,
) -> Value {
    json!({
        "table_id": id.to_string(),
        "state": Value::Null,
        "sampled_at": sampled_at.map(stamp),
        "confirmed_at": confirmed_at.map(stamp),
        "findings": [],
        "recommendations": [],
    })
}

/// The health wire form: state, sample instants, findings, recommendations.
fn document(
    id: TableId,
    stored: &StoredTableSample,
    confirmed_at: DateTime<Utc>,
    health: &TableHealth,
    recommendations: &[Recommendation],
    assessed: Option<&AssessedPreview>,
) -> Value {
    json!({
        "table_id": id.to_string(),
        "state": health.state.as_str(),
        "sampled_at": stamp(stored.sampled_at),
        "confirmed_at": stamp(confirmed_at),
        "findings": health.findings.iter().map(finding).collect::<Vec<_>>(),
        "recommendations": recommendations
            .iter()
            .map(|recommendation| recommendation_document(id, recommendation, assessed))
            .collect::<Vec<_>>(),
    })
}

fn finding(finding: &Finding) -> Value {
    json!({
        "kind": finding.kind,
        "severity": finding.severity,
        "measured": finding.measured.to_string(),
        "threshold": finding.threshold.to_string(),
        "unit": finding.unit(),
        "reason": finding.reason,
    })
}

/// One recommendation; an actionable one carries the preview and its estimate.
fn recommendation_document(
    table: TableId,
    recommendation: &Recommendation,
    assessed: Option<&AssessedPreview>,
) -> Value {
    let mut value = json!({
        "kind": recommendation.kind,
        "action": recommendation.action.as_str(),
        "advisory": recommendation.advisory.is_some(),
        "reason": recommendation.advisory,
        "preview": Value::Null,
        "outcome": Value::Null,
        "no_op_reason": Value::Null,
        "estimate": Value::Null,
    });
    let Some(assessed) = assessed.filter(|_| recommendation.advisory.is_none()) else {
        return value;
    };
    let (would, no_op_reason) = outcome(&assessed.preview);
    value["preview"] = json!({"table_id": table.to_string()});
    value["outcome"] = json!(would);
    value["no_op_reason"] = json!(no_op_reason);
    value["estimate"] = estimate(&assessed.preview.estimate);
    value
}

fn estimate(estimate: &Estimate) -> Value {
    json!({
        "tasks": estimate.tasks.to_string(),
        "files": estimate.files.to_string(),
        "bytes": estimate.bytes.to_string(),
        "files_removed": estimate.files_removed.to_string(),
        "duration_millis": estimate.duration_millis.to_string(),
    })
}

/// The stored sample as the health model's input.
///
/// The ledger stores counters as `bigint` and the histogram and partitions as
/// JSON; a row that cannot be presented is a ledger defect, not a client
/// error.
fn sample_model(stored: &StoredTableSample) -> Result<TableSample, ApiError> {
    Ok(TableSample {
        metadata_location: stored.metadata_location.clone(),
        snapshot_id: stored.snapshot_id,
        last_operation: stored.last_operation.clone(),
        record_count: counter(stored.record_count)?,
        data_files: counter(stored.data_files)?,
        data_bytes: counter(stored.data_bytes)?,
        delete_files: counter(stored.delete_files)?,
        delete_bytes: counter(stored.delete_bytes)?,
        position_delete_files: counter(stored.position_delete_files)?,
        equality_delete_files: counter(stored.equality_delete_files)?,
        position_delete_records: counter(stored.position_delete_records)?,
        equality_delete_records: counter(stored.equality_delete_records)?,
        manifest_count: counter(stored.manifest_count)?,
        data_manifest_count: counter(stored.data_manifest_count)?,
        manifest_bytes: counter(stored.manifest_bytes)?,
        partition_count: counter(stored.partition_count)?,
        max_partition_files: counter(stored.max_partition_files)?,
        small_files: counter(stored.small_files)?,
        target_file_size_bytes: counter(stored.target_file_size_bytes)?,
        size_histogram: serde_json::from_value(stored.size_histogram.clone())
            .map_err(|_| ApiError::internal())?,
        top_partitions: serde_json::from_value(stored.top_partitions.clone())
            .map_err(|_| ApiError::internal())?,
        snapshot_count: counter(stored.snapshot_count)?,
        oldest_snapshot_at: stored.oldest_snapshot_at,
        newest_snapshot_at: stored.newest_snapshot_at,
    })
}

fn counter(value: i64) -> Result<u64, ApiError> {
    u64::try_from(value).map_err(|_| ApiError::internal())
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}
