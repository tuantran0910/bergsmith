//! Table health findings and their recommendations (BS-028.T4, BS-028.T2).
//!
//! The newest metadata-only sample is evaluated with ADR 0059's model through
//! [`crate::health::assess`], so the page, the list filter, and schedule
//! admission share one evaluation and one threshold source. A table's state is
//! the worst evaluation across its enabled policies' threshold sets, and each
//! finding names the policy that supplied its thresholds.
//!
//! Each finding carries the action that resolves it. Actions the planner can
//! run today get the prefilled preview that would assess them and its estimate;
//! the rest say why they cannot run yet. The response is derived from the
//! ledger, except the estimate, which loads the table through the same preview
//! path a submission uses.
//!
//! Findings are metadata a viewer may already read, so they need inventory
//! visibility only. The catalog assessment a runnable recommendation carries
//! needs `jobs:submit`, and a recommendation without that grant says so instead
//! of omitting itself.
//!
//! Counters cross the API as decimal strings, as every other counter does.

use axum::Json;
use axum::extract::{Path, State};
use bergsmith_common::ids::TableId;
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_maintenance::{
    AssessmentState, Estimate, Finding, Recommendation, TableHealth, recommend,
};
use bergsmith_persistence::grants;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::health::ThresholdProvenance;
use crate::preview::AssessedPreview;

use super::pagination::PageQuery;
use super::preview::{MissingTable, failed, outcome, resolve};
use super::resources::{self, Resource};
use super::{ApiError, ApiState, Caller};

/// The health of `id` and the recommended action for each finding.
///
/// A table without a sample has no state and no recommendation, and the
/// response is served without a catalog request. A runnable recommendation's
/// estimate is loaded only when the caller may submit jobs.
pub(super) async fn table(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = TableId::from_uuid(id.map_err(|_| ApiError::invalid())?.0);
    // Visibility comes first: a table outside the caller's inventory reads as
    // unknown, so a missing grant cannot confirm that the table exists.
    let _ = resources::read(
        &state,
        &caller.0,
        Resource::Table,
        PageQuery::default(),
        Some(*id.as_uuid()),
    )
    .await?;
    let (assessment, can_submit) = {
        let mut conn = state.database.pool().acquire().await?;
        let can_submit = grants::has_permission(
            &mut conn,
            caller.organization_id,
            caller.principal_id,
            Permission::SubmitJobs,
            ResourceScope::Table(id),
        )
        .await?;
        let mut assessments = crate::health::assess(
            &mut conn,
            caller.organization_id,
            std::slice::from_ref(&id),
            Utc::now(),
        )
        .await?;
        (assessments.remove(&id), can_submit)
    };
    let Some(assessment) = assessment else {
        return Ok(Json(unassessed(id, None, None)));
    };
    let (sampled_at, confirmed_at) = (assessment.sampled_at, assessment.confirmed_at);
    let (Some(health), Some(source)) = (assessment.health, assessment.source) else {
        return Ok(Json(unassessed(id, sampled_at, confirmed_at)));
    };
    let recommendations: Vec<Recommendation> = health
        .findings
        .iter()
        .map(|finding| recommend(finding.kind))
        .collect();
    let needs_estimate = can_submit
        && recommendations
            .iter()
            .any(|recommendation| recommendation.advisory.is_none());
    let assessed = if needs_estimate {
        let target = resolve(
            &state,
            &caller,
            id,
            Permission::SubmitJobs,
            MissingTable::Forbidden,
        )
        .await?;
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
        (sampled_at, confirmed_at),
        &health,
        &source,
        &recommendations,
        assessed.as_ref(),
        can_submit,
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
    instants: (Option<DateTime<Utc>>, Option<DateTime<Utc>>),
    health: &TableHealth,
    source: &ThresholdProvenance,
    recommendations: &[Recommendation],
    assessed: Option<&AssessedPreview>,
    can_submit: bool,
) -> Value {
    json!({
        "table_id": id.to_string(),
        "state": health.state.as_str(),
        "sampled_at": instants.0.map(stamp),
        "confirmed_at": instants.1.map(stamp),
        "findings": health.findings.iter()
            .map(|finding| self::finding(finding, source))
            .collect::<Vec<_>>(),
        "recommendations": recommendations
            .iter()
            .map(|recommendation| recommendation_document(id, recommendation, assessed, can_submit))
            .collect::<Vec<_>>(),
    })
}

fn finding(finding: &Finding, source: &ThresholdProvenance) -> Value {
    json!({
        "kind": finding.kind,
        "severity": finding.severity,
        "measured": finding.measured.to_string(),
        "threshold": finding.threshold.to_string(),
        "unit": finding.unit(),
        "reason": finding.reason,
        "source": source_document(source),
    })
}

/// Where a finding's thresholds came from, named so the reason is checkable.
fn source_document(source: &ThresholdProvenance) -> Value {
    match source {
        ThresholdProvenance::Defaults => json!({"kind": "defaults"}),
        ThresholdProvenance::Policy { id, name } => {
            json!({"kind": "policy", "policy_id": id.to_string(), "policy_name": name})
        }
    }
}

/// One recommendation; an actionable one carries the preview and its estimate
/// when the caller may submit jobs.
fn recommendation_document(
    table: TableId,
    recommendation: &Recommendation,
    assessed: Option<&AssessedPreview>,
    can_submit: bool,
) -> Value {
    let assessment = AssessmentState::of(recommendation, can_submit);
    let mut value = json!({
        "kind": recommendation.kind,
        "action": recommendation.action.as_str(),
        "advisory": recommendation.advisory.is_some(),
        "reason": recommendation.advisory,
        "assessment": assessment.as_str(),
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

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}
