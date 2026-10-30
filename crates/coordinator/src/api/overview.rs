//! The landing overview (BS-027.T3, BS-032.T7).
//!
//! One authorized read answers "what needs attention?" from the ledger: jobs
//! that ended in the last day, jobs whose publication is unresolved or whose
//! work a conflict discarded, tables with no sample or a decision pending,
//! schedule evaluations that are late or folded, and registered worker
//! capacity. Job outcomes also carry the previous window's counts, so the page
//! can show how each figure moved (BS-033.T7). Every figure follows the
//! caller's grants; the schedule and worker
//! sections are `null` when the caller may not read them, rather than failing
//! the page.

use axum::Json;
use axum::extract::State;
use bergsmith_common::permission::{Permission, ResourceScope};
use bergsmith_persistence::grants;
use bergsmith_persistence::overview;
use chrono::{TimeDelta, Utc};
use serde_json::{Value, json};

use super::{ApiError, ApiState, Caller};

/// How far back the outcome counts reach.
const JOB_WINDOW_HOURS: i64 = 24;

/// How far back an unresolved or discarded job stays on the page.
const ATTENTION_WINDOW_DAYS: i64 = 30;

/// The most attention items one page lists.
const ATTENTION_LIMIT: i64 = 5;

/// The organization's overview.
pub(super) async fn overview(
    State(state): State<ApiState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    let now = Utc::now();
    let roles: Vec<String> = Permission::ViewJobs
        .roles()
        .iter()
        .map(|role| role.as_str().to_owned())
        .collect();
    let job_since = now - TimeDelta::hours(JOB_WINDOW_HOURS);
    let attention_since = now - TimeDelta::days(ATTENTION_WINDOW_DAYS);

    let mut conn = state.database.pool().acquire().await?;
    let outcomes = overview::job_outcomes(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        &roles,
        job_since,
        None,
    )
    .await?;
    let previous = overview::job_outcomes(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        &roles,
        job_since - TimeDelta::hours(JOB_WINDOW_HOURS),
        Some(job_since),
    )
    .await?;
    let attention = overview::attention(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        &roles,
        attention_since,
        ATTENTION_LIMIT,
    )
    .await?;
    let attention_summary = overview::attention_summary(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        &roles,
        attention_since,
    )
    .await?;
    let tables = overview::tables(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        &roles,
        attention_since,
    )
    .await?;
    let schedules = if grants::has_permission(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        Permission::ManageSchedules,
        ResourceScope::Organization,
    )
    .await?
    {
        Some(overview::schedules(&mut conn, caller.organization_id).await?)
    } else {
        None
    };
    let workers = if grants::has_permission(
        &mut conn,
        caller.organization_id,
        caller.principal_id,
        Permission::ManageWorkers,
        ResourceScope::Organization,
    )
    .await?
    {
        Some(overview::workers(&mut conn, caller.organization_id).await?)
    } else {
        None
    };

    Ok(Json(report(
        outcomes,
        previous,
        attention_summary,
        &attention,
        tables,
        schedules,
        workers,
    )))
}

/// The wire form of the overview.
fn report(
    outcomes: overview::JobOutcomes,
    previous: overview::JobOutcomes,
    attention_summary: overview::AttentionSummary,
    attention: &[overview::AttentionJob],
    tables: overview::TableOverview,
    schedules: Option<overview::ScheduleOverview>,
    workers: Option<overview::WorkerOverview>,
) -> Value {
    let mut jobs = outcome_counts(outcomes);
    jobs.insert("window_hours".to_owned(), json!(JOB_WINDOW_HOURS));
    jobs.insert(
        "previous".to_owned(),
        Value::Object(outcome_counts(previous)),
    );
    json!({
        "jobs": jobs,
        "attention": {
            "unresolved_publications": counter(attention_summary.unresolved_publications),
            "oldest_unresolved_age_seconds": counter(
                attention_summary.oldest_unresolved_age_seconds,
            ),
            "terminal_jobs": counter(attention_summary.terminal_jobs),
            "truncated": i64::try_from(attention.len()).unwrap_or(i64::MAX)
                < attention_summary.unresolved_publications + attention_summary.terminal_jobs,
            "items": attention
                .iter()
                .map(|job| json!({
                    "job_id": job.job_id,
                    "table_id": job.table_id,
                    "state": job.state.as_str(),
                    "reason": job.reason.as_str(),
                    "age_seconds": counter(job.age_seconds),
                }))
                .collect::<Vec<_>>(),
        },
        "tables": {
            "window_days": ATTENTION_WINDOW_DAYS,
            "total": counter(tables.total),
            "never_sampled": counter(tables.never_sampled),
            "latest_job_attention": counter(tables.latest_job_attention),
        },
        "schedules": schedules.map(|s| json!({
            "total": counter(s.total),
            "enabled": counter(s.enabled),
            "overdue": counter(s.overdue),
            "oldest_overdue_age_seconds": counter(s.oldest_overdue_age_seconds),
            "coalesced_last_day": counter(s.coalesced),
            "skipped_last_day": counter(s.skipped),
        })),
        "workers": workers.map(|w| json!({
            "sessions": counter(w.sessions),
            "live_sessions": counter(w.live_sessions),
            "memory_bytes": counter(w.memory_bytes),
            "cpu_millis": counter(w.cpu_millis),
            "running_attempts": counter(w.running_attempts),
        })),
    })
}

/// One window's job outcomes: ended, by state, and the conflict rate.
fn outcome_counts(outcomes: overview::JobOutcomes) -> serde_json::Map<String, Value> {
    let ended = outcomes.ended();
    let mut counts = serde_json::Map::new();
    counts.insert("ended".to_owned(), json!(counter(ended)));
    counts.insert(
        "by_state".to_owned(),
        Value::Object(
            outcomes
                .by_state()
                .into_iter()
                .map(|(state, count)| (state.to_owned(), json!(counter(count))))
                .collect(),
        ),
    );
    counts.insert(
        "conflict_rate_percent".to_owned(),
        json!(percent(outcomes.replanned, ended)),
    );
    counts
}

/// A percentage with one decimal, as a decimal string.
fn percent(part: i64, whole: i64) -> String {
    if whole == 0 {
        return "0.0".to_owned();
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "job counts stay far below 2^53, and one decimal is the contract"
    )]
    let value = part as f64 * 100.0 / whole as f64;
    format!("{value:.1}")
}

/// A counter as a decimal string.
fn counter(value: i64) -> String {
    value.to_string()
}
