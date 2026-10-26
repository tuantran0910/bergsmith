//! Health on the tables list: the evaluated state, its filter, and its order
//! (BS-028.T4).

use super::*;

use bergsmith_common::ids::CatalogConnectionId;
use bergsmith_persistence::{schedules, statistics};
use chrono::{DateTime, Utc};

/// A sample that is healthy under the documented defaults: 200 MiB average
/// files, four snapshots, no deletes, and freshly confirmed metadata.
fn healthy_sample(
    connection: CatalogConnectionId,
    at: DateTime<Utc>,
) -> statistics::NewTableSample {
    statistics::NewTableSample {
        catalog_connection_id: connection,
        sampled_at: at,
        metadata_location: "s3://bucket/table/metadata/v1.metadata.json".to_owned(),
        snapshot_id: Some(1),
        last_operation: Some("append".to_owned()),
        record_count: 5,
        data_files: 5,
        data_bytes: 5 * 200 * 1024 * 1024,
        delete_files: 0,
        delete_bytes: 0,
        position_delete_files: 0,
        equality_delete_files: 0,
        position_delete_records: 0,
        equality_delete_records: 0,
        manifest_count: 2,
        data_manifest_count: 2,
        manifest_bytes: 2_048,
        partition_count: 0,
        max_partition_files: 0,
        small_files: 0,
        target_file_size_bytes: 512 * 1024 * 1024,
        snapshot_count: 4,
        oldest_snapshot_at: Some(at),
        newest_snapshot_at: Some(at),
        size_histogram: json!([]),
        top_partitions: json!([]),
    }
}

/// A second table in the tenant's organization, on its connection.
async fn another_table(
    conn: &mut PgConnection,
    tenant: &support::Tenant,
    name: &str,
) -> bergsmith_common::ids::TableId {
    tables::resolve_table(
        conn,
        tenant.organization,
        &support::alias(tenant.connection, name),
        Uuid::now_v7(),
    )
    .await
    .unwrap()
    .id
}

async fn record_sample(
    conn: &mut PgConnection,
    tenant: &support::Tenant,
    table: bergsmith_common::ids::TableId,
    sample: &statistics::NewTableSample,
) {
    assert!(
        statistics::record(conn, tenant.organization, table, sample)
            .await
            .unwrap()
    );
    statistics::record_check(conn, tenant.organization, table, sample.sampled_at)
        .await
        .unwrap();
}

fn instant() -> DateTime<Utc> {
    DateTime::from_timestamp_micros(Utc::now().timestamp_micros()).unwrap()
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_list_and_detail_carry_each_tables_health(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let unsampled = another_table(&mut conn, &tenant, "archive").await;
    let at = instant();
    record_sample(
        &mut conn,
        &tenant,
        tenant.table,
        &health_sample(tenant.connection, at),
    )
    .await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, body) = browser.request("GET", "/api/v1/tables", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let sampled = items
        .iter()
        .find(|item| item["id"] == tenant.table.to_string())
        .unwrap();
    assert_eq!(sampled["health"]["state"], "critical");
    assert_eq!(
        sampled["health"]["sampled_at"],
        at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    );
    assert_eq!(
        sampled["health"]["confirmed_at"],
        at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    );
    let untouched = items
        .iter()
        .find(|item| item["id"] == unsampled.to_string())
        .unwrap();
    assert_eq!(untouched["health"]["state"], Value::Null);
    assert_eq!(untouched["health"]["sampled_at"], Value::Null);
    assert_eq!(untouched["health"]["confirmed_at"], Value::Null);

    // The detail resource carries the same health as its list row.
    let (status, detail) = browser
        .request(
            "GET",
            &format!("/api/v1/tables/{}", tenant.table),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail, *sampled);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_health_filter_is_exact_and_orders_worst_first(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let warning = another_table(&mut conn, &tenant, "warned").await;
    let unassessed = another_table(&mut conn, &tenant, "unsampled").await;
    let at = instant();
    record_sample(
        &mut conn,
        &tenant,
        tenant.table,
        &health_sample(tenant.connection, at),
    )
    .await;
    record_sample(
        &mut conn,
        &tenant,
        warning,
        &healthy_sample(tenant.connection, at),
    )
    .await;
    // Under the defaults that sample is healthy, so the order below shows all
    // three states: a critical sample, a healthy one, and an unassessed table.
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, body) = browser
        .request("GET", "/api/v1/tables?sort=health", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let order: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        vec![
            tenant.table.to_string(),
            warning.to_string(),
            unassessed.to_string()
        ]
    );
    let states: Vec<Value> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["health"]["state"].clone())
        .collect();
    assert_eq!(
        states,
        vec![json!("critical"), json!("healthy"), Value::Null]
    );

    // The filter keeps exactly one state, still worst first.
    let (status, body) = browser
        .request("GET", "/api/v1/tables?health=healthy", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], warning.to_string());

    let (status, body) = browser
        .request("GET", "/api/v1/tables?health=unassessed", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["id"], unassessed.to_string());

    // The cursor continues the health order across pages.
    let (status, first) = browser
        .request("GET", "/api/v1/tables?sort=health&limit=1", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["items"][0]["id"], tenant.table.to_string());
    let cursor = first["next_cursor"].as_str().unwrap();
    let (status, second) = browser
        .request(
            "GET",
            &format!("/api/v1/tables?sort=health&limit=1&cursor={cursor}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["items"][0]["id"], warning.to_string());

    // A plain-list cursor cannot be resumed under the health order, and an
    // unknown value is a request error rather than an ignored filter.
    let (status, plain) = browser
        .request("GET", "/api/v1/tables?limit=1", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{plain}");
    let plain_cursor = plain["next_cursor"].as_str().unwrap();
    let (status, _) = browser
        .request(
            "GET",
            &format!("/api/v1/tables?sort=health&limit=1&cursor={plain_cursor}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A cursor also repeats the filter it was produced under.
    let (status, ordered) = browser
        .request("GET", "/api/v1/tables?sort=health&limit=1", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{ordered}");
    let ordered_cursor = ordered["next_cursor"].as_str().unwrap();
    let (status, _) = browser
        .request(
            "GET",
            &format!("/api/v1/tables?health=critical&limit=1&cursor={ordered_cursor}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    for query in ["health=broken", "sort=name"] {
        let (status, _) = browser
            .request("GET", &format!("/api/v1/tables?{query}"), None, None)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn corrupt_threshold_sets_are_skipped_and_defaults_apply_when_all_are_corrupt(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let corrupt_only = another_table(&mut conn, &tenant, "corrupt-only").await;
    let at = instant();
    record_sample(
        &mut conn,
        &tenant,
        tenant.table,
        &healthy_sample(tenant.connection, at),
    )
    .await;
    record_sample(
        &mut conn,
        &tenant,
        corrupt_only,
        &healthy_sample(tenant.connection, at),
    )
    .await;
    let service = support::service_principal(&mut conn, tenant.organization, "scheduler").await;
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: service,
            role: Role::MaintenanceService,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, strict) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({
                "name": "strict",
                "health_thresholds": {
                    "small_files": {"warning": 2_147_483_648_u64, "critical": 1_073_741_824_u64}
                }
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{strict}");
    let (status, broken) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({"name": "broken"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{broken}");
    for (policy, table) in [
        (strict["id"].clone(), tenant.table),
        (broken["id"].clone(), tenant.table),
        (broken["id"].clone(), corrupt_only),
    ] {
        let (status, schedule) = browser
            .request(
                "POST",
                "/api/v1/schedules",
                Some(json!({
                    "policy_id": policy,
                    "table_id": table,
                    "cron_expression": "0 2 * * *",
                    "time_zone": "UTC",
                    "service_principal_id": service,
                })),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{schedule}");
    }

    // Corrupt the stored set behind the API's back, as an upgrade or a manual
    // edit would; the policy revision and validation accepted it once.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query(
        r#"UPDATE maintenance_policies SET health_thresholds =
           '{"small_files": {"warning": "high"}}'::jsonb WHERE name = 'broken'"#,
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);

    // The valid set still decides the state on its table, and the corrupt set
    // is skipped rather than failing the response.
    let (status, body) = browser
        .request(
            "GET",
            &format!("/api/v1/tables/{}", tenant.table),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["health"]["state"], "critical");

    // A table with only a corrupt set falls back to the documented defaults,
    // which judge the sample healthy.
    let (status, body) = browser
        .request("GET", &format!("/api/v1/tables/{corrupt_only}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["health"]["state"], "healthy");

    // Both rows remain listable, worst first.
    let (status, body) = browser
        .request("GET", "/api/v1/tables?sort=health", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["id"], tenant.table.to_string());
    assert_eq!(body["items"][0]["health"]["state"], "critical");
    assert_eq!(body["items"][1]["health"]["state"], "healthy");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_health_page_resumes_by_state_not_by_identifier(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    // Identity ids follow creation order, so the healthy table sorts before the
    // critical one; a continuation that resumed after the critical id would
    // skip it.
    let healthy = another_table(&mut conn, &tenant, "healthy-table").await;
    let critical = another_table(&mut conn, &tenant, "critical-table").await;
    let at = instant();
    record_sample(
        &mut conn,
        &tenant,
        healthy,
        &healthy_sample(tenant.connection, at),
    )
    .await;
    record_sample(
        &mut conn,
        &tenant,
        critical,
        &health_sample(tenant.connection, at),
    )
    .await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, first) = browser
        .request("GET", "/api/v1/tables?sort=health&limit=1", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["items"][0]["id"], critical.to_string());
    let cursor = first["next_cursor"].as_str().unwrap();
    let (status, second) = browser
        .request(
            "GET",
            &format!("/api/v1/tables?sort=health&limit=1&cursor={cursor}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(
        second["items"][0]["id"],
        healthy.to_string(),
        "a page must resume by state, not by identifier"
    );
    let cursor = second["next_cursor"].as_str().unwrap();
    let (status, third) = browser
        .request(
            "GET",
            &format!("/api/v1/tables?sort=health&limit=1&cursor={cursor}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{third}");
    assert_eq!(third["items"][0]["id"], tenant.table.to_string());
    assert!(third["next_cursor"].is_null());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_enabled_policys_thresholds_drive_the_state_and_are_named(pool: PgPool) {
    // The recommendations endpoint assesses an actionable finding through the
    // same catalog path a submission uses, so the connection answers it.
    let fixture = fixture_table().await;
    let mut server = mockito::Server::new_async().await;
    let config = catalog_config_mock(&mut server).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme", &server.url(), fixture.metadata().uuid()).await;
    let load = load_mock(&mut server, &fixture, "events").await;
    let at = instant();
    record_sample(
        &mut conn,
        &tenant,
        tenant.table,
        &healthy_sample(tenant.connection, at),
    )
    .await;
    let service = support::service_principal(&mut conn, tenant.organization, "scheduler").await;
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: service,
            role: Role::MaintenanceService,
            scope: GrantScope::table(tenant.table),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    // Healthy under the documented defaults.
    let (status, body) = browser
        .request(
            "GET",
            &format!("/api/v1/tables/{}", tenant.table),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["health"]["state"], "healthy");

    // A policy whose small-file bounds are stricter than the defaults.
    let (status, policy) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({
                "name": "strict",
                "health_thresholds": {
                    "small_files": {"warning": 2_147_483_648_u64, "critical": 1_073_741_824_u64}
                }
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{policy}");
    let (status, schedule) = browser
        .request(
            "POST",
            "/api/v1/schedules",
            Some(json!({
                "policy_id": policy["id"],
                "table_id": tenant.table,
                "cron_expression": "0 2 * * *",
                "time_zone": "UTC",
                "service_principal_id": service,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{schedule}");

    // The page reports the worst enabled set: the same 200 MiB average is now
    // critical, and the finding names the policy that supplied the bounds.
    let (status, body) = browser
        .request(
            "GET",
            &format!("/api/v1/tables/{}", tenant.table),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["health"]["state"], "critical");

    let (status, health) = browser
        .request(
            "GET",
            &format!("/api/v1/tables/{}/recommendations", tenant.table),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{health}");
    assert_eq!(health["state"], "critical");
    assert_eq!(health["findings"][0]["kind"], "small_files");
    assert_eq!(health["findings"][0]["severity"], "critical");
    assert_eq!(
        health["findings"][0]["source"],
        json!({
            "kind": "policy",
            "policy_id": policy["id"],
            "policy_name": "strict"
        })
    );

    // A disabled policy contributes no threshold set, so the defaults apply
    // again; the same measurement is healthy. Editing a policy touches no
    // catalog, which keeps this check independent of the service route.
    let (status, disabled) = browser
        .request(
            "PATCH",
            &format!("/api/v1/policies/{}", policy["id"].as_str().unwrap()),
            Some(json!({
                "revision": policy["revision"],
                "enabled": false
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{disabled}");
    let (status, body) = browser
        .request(
            "GET",
            &format!("/api/v1/tables/{}", tenant.table),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["health"]["state"], "healthy");

    // The policy's stored override is still readable, so the change is not a
    // lost write hidden by the page.
    let mut conn = pool.acquire().await.unwrap();
    let policy_id: PolicyId = policy["id"].as_str().unwrap().parse().unwrap();
    let stored = schedules::policy(&mut conn, tenant.organization, policy_id)
        .await
        .unwrap();
    assert_eq!(stored.health_thresholds.small_files.warning, 2_147_483_648);
    config.assert_async().await;
    load.assert_async().await;
}
