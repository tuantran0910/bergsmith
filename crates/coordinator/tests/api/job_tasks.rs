use super::*;

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn tasks_expose_attempt_failure_duration_and_outputs(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "triage").await;
    let other = support::tenant(&mut conn, "foreign").await;
    let job = support::job(&mut conn, &tenant).await;
    let tasks = support::plan(&mut conn, job, 2).await;
    support::force_job_state(&mut conn, job, JobState::Running).await;
    let failed = support::grant(&mut conn, tasks[0]).await.unwrap();
    let completed = support::grant(&mut conn, tasks[1]).await.unwrap();
    sqlx::query(
        "INSERT INTO attempt_failures (attempt_id, reason_code, disposition, job_state) \
         VALUES ($1, 'ProcessMemoryLimitExceeded', 'PermanentFailure', 'Failed')",
    )
    .bind(failed.id.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE attempts SET state = 'Failed', finished_at = granted_at + interval '4 seconds' \
         WHERE id = $1",
    )
    .bind(failed.id.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("UPDATE tasks SET state = 'Failed' WHERE id = $1")
        .bind(tasks[0].as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO receipts (attempt_id, task_id, content, content_hash) VALUES ($1,$2,'[]',$3)",
    )
    .bind(completed.id.as_uuid())
    .bind(tasks[1].as_uuid())
    .bind(support::digest('b'))
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("INSERT INTO attempt_output_prefixes (attempt_id, prefix) VALUES ($1,$2)")
        .bind(completed.id.as_uuid())
        .bind(format!("s3://fixture/{}/", completed.id))
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO attempt_output_objects (attempt_id, path, size_bytes) VALUES ($1,$2,9007199254740993)")
        .bind(completed.id.as_uuid())
        .bind("s3://fixture/output.parquet")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("UPDATE attempts SET state = 'Accepted', finished_at = granted_at + interval '9 seconds' WHERE id = $1")
        .bind(completed.id.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);

    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let uri = format!("/api/v1/jobs/{job}/tasks");
    let (status, body) = browser.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    check_contract("GET", &uri, status, &body);
    assert_eq!(body["next_cursor"], Value::Null);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["ordinal"], 0);
    assert_eq!(items[0]["state"], "Failed");
    assert_eq!(items[0]["attempts_started"], 1);
    assert_eq!(items[0]["retry_not_before"], Value::Null);
    assert_eq!(items[0]["output_files"], "0");
    assert_eq!(items[0]["output_bytes"], "0");
    let attempt = &items[0]["attempts"][0];
    assert_eq!(attempt["generation"], 1);
    assert_eq!(attempt["state"], "Failed");
    assert_eq!(attempt["pool"], "test");
    assert_eq!(attempt["worker_name"], "test");
    assert_eq!(attempt["failure_reason"], "ProcessMemoryLimitExceeded");
    assert_eq!(attempt["failure_disposition"], "PermanentFailure");
    assert_eq!(attempt["duration_millis"], "4000");
    assert!(attempt["finished_at"].is_string());
    assert_eq!(items[1]["state"], "Leased");
    assert_eq!(items[1]["output_files"], "1");
    assert_eq!(items[1]["output_bytes"], "9007199254740993");
    let completed_attempt = &items[1]["attempts"][0];
    assert_eq!(completed_attempt["state"], "Accepted");
    assert_eq!(completed_attempt["failure_reason"], Value::Null);
    assert_eq!(completed_attempt["output_bytes"], "9007199254740993");

    // The page is ordered by plan ordinal and the cursor continues there.
    let (status, page) = browser
        .request("GET", &format!("{uri}?limit=1"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let cursor = page["next_cursor"].as_str().unwrap();
    let (status, rest) = browser
        .request("GET", &format!("{uri}?limit=1&cursor={cursor}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{rest}");
    assert_eq!(rest["items"][0]["ordinal"], 1);
    assert_eq!(rest["next_cursor"], Value::Null);

    // A viewer with the table grant sees the same triage as the requester.
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    let visible = viewer.request("GET", &uri, None, None).await.1;
    assert_eq!(visible["items"][0]["attempts"], items[0]["attempts"]);

    // Unknown and foreign jobs are both absent.
    let unknown = format!("/api/v1/jobs/{}/tasks", Uuid::now_v7());
    assert_eq!(
        browser.request("GET", &unknown, None, None).await.0,
        StatusCode::NOT_FOUND
    );
    let foreign = Browser::new(&pool, &other, other.user, "alice").await;
    assert_eq!(
        foreign.request("GET", &uri, None, None).await.0,
        StatusCode::NOT_FOUND
    );

    // A malformed cursor is a client error, not an empty page.
    assert_eq!(
        browser
            .request("GET", &format!("{uri}?cursor=not-a-cursor"), None, None)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}
