//! Maintenance impact contracts (BS-027.T2).
//!
//! The ledger is built directly: a job, an immutable plan, accepted receipts
//! with verified output objects, and a forced terminal state. Work that was
//! never accepted must not contribute, and the grant boundary must hide jobs
//! the caller may not read.

use super::*;

use bergsmith_common::ids::{PlanId, TaskId};
use bergsmith_persistence::plans::{self, NewPlan};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};

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
fn instant(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Three terminal jobs: one published, one discarded after a conflict, and
/// one stopped by cancellation.
async fn ledger(conn: &mut PgConnection, tenant: &support::Tenant) -> (JobId, JobId, JobId) {
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
    support::force_job_state(conn, published, JobState::Succeeded).await;

    // The retained delete file applies again here, so the range total must
    // count two applications, not one distinct path.
    let discarded = support::job(conn, tenant).await;
    record_plan(
        conn,
        discarded,
        json!({"tasks": [task(TaskId::generate(), 100, Some("shared-delete"))]}),
        1,
        4_000,
    )
    .await;
    support::force_job_state(conn, discarded, JobState::ReplanRequired).await;

    let stopped = support::job(conn, tenant).await;
    record_plan(
        conn,
        stopped,
        json!({"tasks": [task(TaskId::generate(), 50, None)]}),
        1,
        800,
    )
    .await;
    support::force_job_state(conn, stopped, JobState::Cancelled).await;
    (published, discarded, stopped)
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn history_and_totals_count_accepted_work_and_discarded_output(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "impact").await;
    let (published, discarded, stopped) = ledger(&mut conn, &tenant).await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let since = instant(Utc::now() - TimeDelta::hours(1));
    let uri = format!("/api/v1/tables/{}/maintenance?since={since}", tenant.table);

    let (status, body) = browser.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["table_id"], tenant.table.to_string());
    let totals = &body["totals"];
    for (field, value) in [
        ("jobs", "3"),
        ("succeeded", "1"),
        ("no_op", "0"),
        ("cancelled", "1"),
        ("replanned", "1"),
        ("failed", "0"),
        ("input_files", "4"),
        ("input_bytes", "177"),
        // Published output only: the discarded and stopped jobs' outputs are
        // reported in their own buckets, so no byte is counted twice.
        ("output_files", "2"),
        ("output_bytes", "2000"),
        ("delete_files_applied", "2"),
        ("discarded_files", "1"),
        ("discarded_bytes", "4000"),
        ("abandoned_files", "1"),
        ("abandoned_bytes", "800"),
    ] {
        assert_eq!(totals[field], value, "{field}: {totals}");
    }

    // Newest first: cancellation came last, the conflict in the middle.
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["job_id"], stopped.to_string());
    assert_eq!(items[0]["state"], "Cancelled");
    assert_eq!(items[0]["input_files"], "1");
    assert_eq!(items[0]["input_bytes"], "50");
    assert_eq!(items[0]["output_bytes"], "800");
    assert_eq!(items[0]["delete_files_applied"], "0");
    assert_eq!(items[1]["job_id"], discarded.to_string());
    assert_eq!(items[1]["state"], "ReplanRequired");
    assert_eq!(items[1]["input_files"], "1");
    assert_eq!(items[1]["input_bytes"], "100");
    assert_eq!(items[1]["output_files"], "1");
    assert_eq!(items[1]["output_bytes"], "4000");
    assert_eq!(items[1]["delete_files_applied"], "1");
    assert_eq!(items[2]["job_id"], published.to_string());
    assert_eq!(items[2]["state"], "Succeeded");
    assert_eq!(items[2]["input_files"], "2");
    assert_eq!(items[2]["input_bytes"], "27");
    assert_eq!(items[2]["output_files"], "2");
    assert_eq!(items[2]["output_bytes"], "2000");
    assert_eq!(items[2]["delete_files_applied"], "1");
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

    // A page of two items continues with the older job, and the cursor keeps
    // the range even when the caller sends a later `since`.
    let (status, first) = browser
        .request("GET", &format!("{uri}&limit=2"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    assert_eq!(first["items"][0]["job_id"], stopped.to_string());
    assert_eq!(first["items"][1]["job_id"], discarded.to_string());
    let cursor = first["next_cursor"].as_str().unwrap();
    let future = instant(Utc::now() + TimeDelta::hours(1));
    let (status, next) = browser
        .request(
            "GET",
            &format!(
                "/api/v1/tables/{}/maintenance?limit=2&cursor={cursor}&since={future}",
                tenant.table
            ),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{next}");
    assert_eq!(next["since"], first["since"]);
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
async fn a_cursor_keeps_the_sub_millisecond_range_start(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "impact-precision").await;
    let base = DateTime::from_timestamp_millis(1_767_225_600_000).unwrap();
    let jobs = [
        support::job(&mut conn, &tenant).await,
        support::job(&mut conn, &tenant).await,
        support::job(&mut conn, &tenant).await,
    ];
    // One job sits just below the cutoff, one exactly on it, and one above.
    for (job, offset) in jobs.iter().zip([0_i64, 500, 600]) {
        let at = base + TimeDelta::microseconds(offset);
        sqlx::query("UPDATE jobs SET created_at = $2, updated_at = $2 WHERE id = $1")
            .bind(job.as_uuid())
            .bind(at)
            .execute(&mut *conn)
            .await
            .unwrap();
        support::force_job_state(&mut conn, *job, JobState::Succeeded).await;
    }
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let since = (base + TimeDelta::microseconds(500)).to_rfc3339_opts(SecondsFormat::Micros, true);
    let uri = format!(
        "/api/v1/tables/{}/maintenance?since={since}&limit=1",
        tenant.table
    );

    let (status, first) = browser.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    // The cutoff at .000500Z includes the two later jobs only.
    assert_eq!(first["totals"]["jobs"], "2");
    let cursor = first["next_cursor"].as_str().unwrap();
    let (status, next) = browser
        .request(
            "GET",
            &format!(
                "/api/v1/tables/{}/maintenance?limit=1&cursor={cursor}",
                tenant.table
            ),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{next}");
    // The cursor keeps the microsecond start, so the job below it stays out
    // and the totals do not change between pages.
    assert_eq!(next["since"], first["since"]);
    assert_eq!(next["totals"]["jobs"], "2", "first {first} next {next}");
    assert_eq!(next["items"][0]["job_id"], jobs[1].to_string());
    assert_ne!(next["items"][0]["job_id"], jobs[0].to_string());
    assert!(next["next_cursor"].is_null());
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
    assert_eq!(body["totals"]["jobs"], "3");

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
    assert_eq!(org["totals"]["jobs"], "3");
    assert_eq!(org["totals"]["input_bytes"], "177");

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
    assert_eq!(org["totals"]["jobs"], "6");
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
