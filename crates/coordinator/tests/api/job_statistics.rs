use super::*;
use bergsmith_common::ids::{PlanId, TaskId};
use bergsmith_persistence::plans::{self, NewPlan};

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn statistics_count_only_accepted_work_and_preserve_integer_precision(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let indexed: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE schemaname = 'public' AND indexname = 'attempt_output_objects_by_attempt')")
        .fetch_one(&mut *conn).await.unwrap();
    assert!(indexed, "job statistics must index accepted output lookups");
    let tenant = support::tenant(&mut conn, "metrics").await;
    let other = support::tenant(&mut conn, "foreign").await;
    let job = support::job(&mut conn, &tenant).await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let uri = format!("/api/v1/jobs/{job}");
    let (status, body) = browser.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["statistics"]["plan_recorded"], false);
    assert!(body["statistics"]["planned_input_bytes"].is_null());
    assert_eq!(body["statistics"]["input_bytes"], "0");
    let mut conn = pool.acquire().await.unwrap();
    let tasks = [TaskId::generate(), TaskId::generate(), TaskId::generate()];
    plans::create_plan(&mut conn, &NewPlan {
        id: PlanId::generate(), job_id: job,
        content: json!({"estimate": {"files": 3, "bytes": 9_007_199_254_741_020_u64}, "tasks": [
            {"task_id": tasks[0], "sources": [{"file_size_bytes": 9_007_199_254_740_993_u64}], "deletes": [{"path": "shared-delete"}]},
            {"task_id": tasks[1], "sources": [{"file_size_bytes": 7}], "deletes": [{"path": "shared-delete"}]},
            {"task_id": tasks[2], "sources": [{"file_size_bytes": 20}], "deletes": [{"path": "unaccepted-delete"}]}
        ]}), content_hash: support::digest('a'), task_ids: tasks.to_vec(), execution_budget: Duration::from_secs(180),
    }).await.unwrap();
    support::force_job_state(&mut conn, job, JobState::Running).await;
    for task in tasks.iter().take(2) {
        let attempt = support::grant(&mut conn, *task).await.unwrap();
        // Fixed ledger fixture: receipt and verified inventory are normally
        // committed together by the worker result acceptance transaction.
        sqlx::query("INSERT INTO receipts (attempt_id, task_id, content, content_hash) VALUES ($1,$2,'[]',$3)")
            .bind(attempt.id.as_uuid()).bind(task.as_uuid()).bind(support::digest('b')).execute(&mut *conn).await.unwrap();
        sqlx::query("INSERT INTO attempt_output_prefixes (attempt_id,prefix) VALUES ($1,$2)")
            .bind(attempt.id.as_uuid())
            .bind(format!("s3://fixture/{task}/"))
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("INSERT INTO attempt_output_objects (attempt_id,path,size_bytes) VALUES ($1,$2,9007199254740993)")
            .bind(attempt.id.as_uuid()).bind(format!("s3://fixture/{task}/output.parquet")).execute(&mut *conn).await.unwrap();
    }
    // An attempt without an accepted receipt must not contribute input files.
    support::grant(&mut conn, tasks[2]).await.unwrap();
    drop(conn);
    let (status, body) = browser.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let s = &body["statistics"];
    for (field, value) in [
        ("planned_input_files", "3"),
        ("planned_input_bytes", "9007199254741020"),
        ("tasks_total", "3"),
        ("tasks_accepted", "2"),
        ("attempts", "3"),
        ("input_files", "2"),
        ("input_bytes", "9007199254741000"),
        ("output_files", "2"),
        ("output_bytes", "18014398509481986"),
        ("delete_files_applied", "1"),
    ] {
        assert_eq!(s[field], value, "{field}: {s}");
    }
    assert_eq!(s["publication_confirmed"], false);
    assert!(s["elapsed_millis"].as_str().unwrap().parse::<u64>().is_ok());
    let foreign = Browser::new(&pool, &other, other.user, "alice").await;
    assert_eq!(
        foreign.request("GET", &uri, None, None).await.0,
        StatusCode::NOT_FOUND
    );
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    let visible = viewer.request("GET", &uri, None, None).await.1;
    assert_eq!(visible["statistics"]["input_bytes"], s["input_bytes"]);
    assert_eq!(visible["statistics"]["output_bytes"], s["output_bytes"]);
    let mut conn = pool.acquire().await.unwrap();
    support::force_job_state(&mut conn, job, JobState::Succeeded).await;
    drop(conn);
    assert_eq!(
        browser.request("GET", &uri, None, None).await.1["statistics"]["publication_confirmed"],
        true
    );
    let list = browser.request("GET", "/api/v1/jobs", None, None).await.1;
    assert!(list["items"][0].get("statistics").is_none());
}
