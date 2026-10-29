//! A job's tasks and attempts for triage (BS-032.T5).
//!
//! `GET /api/v1/jobs/{id}/tasks` answers the question the job detail cannot:
//! which task failed, on which attempt, with what reason, how long it ran, and
//! what it produced. Authorization is the job detail's own predicate, so an
//! unknown or invisible job is 404 exactly as for `GET /api/v1/jobs/{id}`.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use bergsmith_common::disposition::FailureDisposition;
use bergsmith_common::ids::{JobId, TaskId};
use bergsmith_persistence::triage;
use serde_json::{Value, json};
use uuid::Uuid;

use super::pagination::{Page, PageQuery};
use super::{ApiError, ApiState, Caller, resources};

#[derive(Default)]
struct TaskAttempts {
    items: Vec<Value>,
    output_files: i64,
    output_bytes: i64,
}

/// One page of a job's tasks, each with its attempts in generation order.
pub(super) async fn tasks(
    State(state): State<ApiState>,
    caller: Caller,
    id: Result<Path<Uuid>, PathRejection>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let id = id.map_err(|_| ApiError::invalid())?.0;
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    // Same authorization and 404 semantics as the job detail endpoint.
    resources::visible_job_state(&state, &caller.0, id).await?;
    let page = Page::parse(
        query,
        caller.0.organization_id,
        caller.0.principal_id,
        "job-tasks",
    )?;
    let after = page
        .after
        .as_deref()
        .map(|raw| raw.parse::<i32>().map_err(|_| ApiError::invalid()))
        .transpose()?;
    let limit = i64::try_from(page.limit + 1).map_err(|_| ApiError::invalid())?;
    let pool = state.database.pool();
    let mut conn = pool.acquire().await?;
    let mut tasks = triage::tasks_after(&mut conn, JobId::from_uuid(id), after, limit).await?;
    let ids: Vec<TaskId> = tasks.iter().map(|task| task.id).collect();
    let attempts = triage::attempts_for(&mut conn, &ids).await?;
    drop(conn);

    let mut by_task: BTreeMap<TaskId, TaskAttempts> = BTreeMap::new();
    for attempt in attempts {
        let duration = attempt
            .finished_at
            .map(|finished| (finished - attempt.granted_at).num_milliseconds().max(0));
        let entry = by_task.entry(attempt.task_id).or_default();
        entry.output_files += attempt.output_files;
        entry.output_bytes += attempt.output_bytes;
        entry.items.push(json!({
            "id": attempt.id.to_string(),
            "generation": attempt.generation,
            "state": attempt.state.as_str(),
            "pool": attempt.pool,
            "worker_name": attempt.worker_name,
            "granted_at": attempt.granted_at,
            "lease_expires_at": attempt.lease_expires_at,
            "finished_at": attempt.finished_at,
            "duration_millis": duration.map(|millis| millis.to_string()),
            "failure_reason": attempt.failure_reason,
            "failure_disposition": attempt.failure_disposition.map(FailureDisposition::as_str),
            "output_files": attempt.output_files.to_string(),
            "output_bytes": attempt.output_bytes.to_string(),
        }));
    }

    let rows = tasks
        .drain(..)
        .map(|task| {
            let attempts = by_task.remove(&task.id).unwrap_or_default();
            let key = task.ordinal.to_string();
            (
                key,
                json!({
                    "id": task.id.to_string(),
                    "ordinal": task.ordinal,
                    "state": task.state.as_str(),
                    "attempts_started": task.attempts_started,
                    "retry_not_before": task.retry_not_before,
                    "updated_at": task.updated_at,
                    "output_files": attempts.output_files.to_string(),
                    "output_bytes": attempts.output_bytes.to_string(),
                    "attempts": attempts.items,
                }),
            )
        })
        .collect();

    Ok(Json(page.finish(rows)?))
}

#[cfg(test)]
mod tests {
    use bergsmith_common::disposition::FailureDisposition;
    use bergsmith_common::task::{AttemptState, TaskState};

    fn documented(schema: &serde_json::Value, pointer: &str) -> Vec<String> {
        schema
            .pointer(pointer)
            .and_then(serde_json::Value::as_array)
            .expect("documented enum")
            .iter()
            .map(|value| value.as_str().expect("enum names").to_owned())
            .collect()
    }

    #[test]
    fn documented_task_and_attempt_enums_match_the_lifecycle() {
        let document: serde_json::Value =
            serde_json::from_str(include_str!("openapi.json")).expect("openapi.json");
        let schemas = &document["components"]["schemas"];
        let tasks: Vec<_> = TaskState::ALL
            .iter()
            .map(|state| state.as_str().to_owned())
            .collect();
        assert_eq!(
            documented(&schemas["JobTask"], "/properties/state/enum"),
            tasks
        );
        let attempts: Vec<_> = AttemptState::ALL
            .iter()
            .map(|state| state.as_str().to_owned())
            .collect();
        assert_eq!(
            documented(&schemas["JobAttempt"], "/properties/state/enum"),
            attempts
        );
        let dispositions: Vec<_> = FailureDisposition::ALL
            .iter()
            .map(|disposition| disposition.as_str().to_owned())
            .collect();
        assert_eq!(
            documented(
                &schemas["JobAttempt"],
                "/properties/failure_disposition/anyOf/0/enum"
            ),
            dispositions
        );
    }

    #[test]
    fn the_tasks_page_stays_bounded_and_ordered() {
        let document: serde_json::Value =
            serde_json::from_str(include_str!("openapi.json")).expect("openapi.json");
        let parameters = &document["paths"]["/api/v1/jobs/{id}/tasks"]["get"]["parameters"];
        assert_eq!(parameters[1]["name"], "limit");
        assert_eq!(parameters[1]["schema"]["maximum"], 100);
        assert_eq!(parameters[2]["name"], "cursor");
        let schema = &document["components"]["schemas"]["JobTaskPage"];
        assert_eq!(
            schema["required"],
            serde_json::json!(["items", "next_cursor"])
        );
    }
}
