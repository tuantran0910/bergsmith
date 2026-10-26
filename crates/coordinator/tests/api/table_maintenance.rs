//! Maintenance impact contracts (BS-027.T2).
//!
//! The ledger is built directly: a job, an immutable plan, accepted receipts
//! with verified output objects, and a forced terminal state. Work that was
//! never accepted must not contribute, and the grant boundary must hide jobs
//! the caller may not read.

use super::*;

use bergsmith_common::ids::{PlanId, TaskId};
use bergsmith_persistence::plans::{self, NewPlan};
use chrono::{SecondsFormat, TimeDelta, Utc};

/// Persists `content` as `job`'s plan, accepts the first `accepted` tasks with
/// one output object each of `output_bytes`, and leaves the job running.
async fn record_plan(
    conn: &mut PgConnection,
    job: JobId,
    content: Value,
    accepted: usize,
    output_bytes: i64,
) {
    let tasks: Vec<TaskId> = content["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["task_id"].as_str().unwrap().parse().unwrap())
        .collect();
    plans::create_plan(
        conn,
        &NewPlan {
            id: PlanId::generate(),
            job_id: job,
            content,
            content_hash: support::digest('a'),
            task_ids: tasks.clone(),
            execution_budget: Duration::from_secs(180),
        },
    )
    .await
    .unwrap();
    support::force_job_state(conn, job, JobState::Running).await;
    for task in tasks.iter().take(accepted) {
        let attempt = support::grant(conn, *task).await.unwrap();
        sqlx::query("INSERT INTO receipts (attempt_id, task_id, content, content_hash) VALUES ($1,$2,'[]',$3)")
            .bind(attempt.id.as_uuid())
            .bind(task.as_uuid())
            .bind(support::digest('b'))
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("INSERT INTO attempt_output_prefixes (attempt_id, prefix) VALUES ($1,$2)")
            .bind(attempt.id.as_uuid())
            .bind(format!("s3://fixture/{task}/"))
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO attempt_output_objects (attempt_id, path, size_bytes) VALUES ($1,$2,$3)",
        )
        .bind(attempt.id.as_uuid())
        .bind(format!("s3://fixture/{task}/output.parquet"))
        .bind(output_bytes)
        .execute(&mut *conn)
        .await
        .unwrap();
    }
}

/// A plan task as the planner records it.
fn task(task: TaskId, source_bytes: u64, delete: Option<&str>) -> Value {
    json!({
        "task_id": task,
        "sources": [{"file_size_bytes": source_bytes}],
        "deletes": delete.map_or_else(|| json!([]), |path| json!([{"path": path}])),
    })
}

/// An RFC 3339 instant with a `Z` suffix, so it needs no query escaping.
fn instant(at: chrono::DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Two terminal jobs, one published and one discarded after a conflict.
async fn ledger(conn: &mut PgConnection, tenant: &support::Tenant) -> (JobId, JobId) {
    let published = support::job(conn, tenant).await;
    let accepted = [TaskId::generate(), TaskId::generate()];
    record_plan(
        conn,
        published,
        json!({"tasks": [
            task(accepted[0], 7, Some("shared-delete")),
            task(accepted[1], 20, Some("shared-delete")),
            task(TaskId::generate(), 21, Some("unaccepted-delete")),
        ]}),
        2,
        1_000,
    )
    .await;
    // Two objects per accepted task would need two inserts; the helper writes
    // one each, so the totals below are the sum of those objects.
    support::force_job_state(conn, published, JobState::Succeeded).await;

    let discarded = support::job(conn, tenant).await;
    record_plan(
        conn,
        discarded,
        json!({"tasks": [task(TaskId::generate(), 100, None)]}),
        1,
        4_000,
    )
    .await;
    support::force_job_state(conn, discarded, JobState::ReplanRequired).await;
    (published, discarded)
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn history_and_totals_count_accepted_work_and_discarded_output(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "impact").await;
    let (published, discarded) = ledger(&mut conn, &tenant).await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let since = instant(Utc::now() - TimeDelta::hours(1));
    let uri = format!("/api/v1/tables/{}/maintenance?since={since}", tenant.table);

    let (status, body) = browser.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["table_id"], tenant.table.to_string());
    let totals = &body["totals"];
    for (field, value) in [
        ("jobs", "2"),
        ("succeeded", "1"),
        ("no_op", "0"),
        ("cancelled", "0"),
        ("replanned", "1"),
        ("failed", "0"),
        ("input_files", "3"),
        ("input_bytes", "127"),
        ("output_files", "3"),
        ("output_bytes", "6000"),
        ("delete_files_applied", "1"),
        ("discarded_files", "1"),
        ("discarded_bytes", "4000"),
    ] {
        assert_eq!(totals[field], value, "{field}: {totals}");
    }

    // Newest first: the discarded job was created after the published one.
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["job_id"], discarded.to_string());
    assert_eq!(items[0]["state"], "ReplanRequired");
    assert_eq!(items[0]["input_files"], "1");
    assert_eq!(items[0]["input_bytes"], "100");
    assert_eq!(items[0]["output_files"], "1");
    assert_eq!(items[0]["output_bytes"], "4000");
    assert_eq!(items[0]["delete_files_applied"], "0");
    assert_eq!(items[1]["job_id"], published.to_string());
    assert_eq!(items[1]["state"], "Succeeded");
    assert_eq!(items[1]["input_files"], "2");
    assert_eq!(items[1]["input_bytes"], "27");
    assert_eq!(items[1]["output_files"], "2");
    assert_eq!(items[1]["output_bytes"], "2000");
    assert_eq!(items[1]["delete_files_applied"], "1");
    assert!(body["next_cursor"].is_null());

    // The organization aggregate reads the same ledger.
    let (status, org) = browser
        .request(
            "GET",
            &format!("/api/v1/statistics/maintenance?since={since}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{org}");
    assert_eq!(org["totals"], *totals);

    // A page of one item continues with the older job.
    let (status, first) = browser
        .request("GET", &format!("{uri}&limit=1"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    assert_eq!(first["items"][0]["job_id"], discarded.to_string());
    let cursor = first["next_cursor"].as_str().unwrap();
    let (status, next) = browser
        .request("GET", &format!("{uri}&limit=1&cursor={cursor}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{next}");
    assert_eq!(next["items"][0]["job_id"], published.to_string());
    assert!(next["next_cursor"].is_null());

    // A range that starts after the jobs excludes them, and the totals agree.
    let later = instant(Utc::now() + TimeDelta::hours(1));
    let (status, empty) = browser
        .request(
            "GET",
            &format!("/api/v1/tables/{}/maintenance?since={later}", tenant.table),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(empty["items"].as_array().unwrap().len(), 0);
    assert_eq!(empty["totals"]["jobs"], "0");
    assert_eq!(empty["totals"]["input_bytes"], "0");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn history_and_organization_totals_respect_grants(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "impact-grants").await;
    ledger(&mut conn, &tenant).await;
    // A second table in the same organization, with its own job.
    let other_table = tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(tenant.connection, "events_two"),
        Uuid::now_v7(),
    )
    .await
    .unwrap()
    .id;
    let other = support::Tenant {
        organization: tenant.organization,
        user: tenant.user,
        connection: tenant.connection,
        table: other_table,
    };
    ledger(&mut conn, &other).await;
    let outsider = support::user(&mut conn, tenant.organization, "bob").await;
    drop(conn);

    let since = instant(Utc::now() - TimeDelta::hours(1));
    let uri = format!("/api/v1/tables/{}/maintenance?since={since}", tenant.table);
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    let (status, body) = viewer.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["totals"]["jobs"], "2");

    // The organization aggregate covers the granted table only.
    let (status, org) = viewer
        .request(
            "GET",
            &format!("/api/v1/statistics/maintenance?since={since}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{org}");
    assert_eq!(org["totals"]["jobs"], "2");
    assert_eq!(org["totals"]["input_bytes"], "127");

    // A principal without a grant cannot tell the table exists.
    let browser = Browser::new(&pool, &tenant, outsider, "bob").await;
    assert_eq!(
        browser.request("GET", &uri, None, None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        browser
            .request(
                "GET",
                &format!("/api/v1/tables/{other_table}/maintenance"),
                None,
                None,
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // An organization-wide administrator sees both tables.
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, org) = admin
        .request(
            "GET",
            &format!("/api/v1/statistics/maintenance?since={since}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{org}");
    assert_eq!(org["totals"]["jobs"], "4");
    assert_eq!(org["totals"]["discarded_files"], "2");

    // Another organization never sees these jobs.
    let mut conn = pool.acquire().await.unwrap();
    let foreign = support::tenant(&mut conn, "impact-foreign").await;
    let foreign_browser = Browser::new(&pool, &foreign, foreign.user, "alice").await;
    let (status, org) = foreign_browser
        .request(
            "GET",
            &format!("/api/v1/statistics/maintenance?since={since}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{org}");
    assert_eq!(org["totals"]["jobs"], "0");
}
