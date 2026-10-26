//! Landing overview contracts (BS-027.T3, BS-032.T7).
//!
//! A fixed ledger drives the counts: terminal jobs by outcome, an unresolved
//! publication, a conflict, inventory coverage, one late schedule evaluation,
//! and no worker sessions. The same ledger is then read by a viewer scoped to
//! one table and by a principal with no grant, so the grants boundary is
//! visible in every section.

use super::*;

use bergsmith_common::ids::ScheduleId;
use chrono::{TimeDelta, Utc};

/// A second table in the tenant's organization.
async fn second_table(conn: &mut PgConnection, tenant: &support::Tenant) -> support::Tenant {
    let table = tables::resolve_table(
        conn,
        tenant.organization,
        &support::alias(tenant.connection, "events_two"),
        Uuid::now_v7(),
    )
    .await
    .unwrap()
    .id;
    support::Tenant {
        organization: tenant.organization,
        user: tenant.user,
        connection: tenant.connection,
        table,
    }
}

/// Creates an hourly schedule for `tenant` and pushes its next due instant ten
/// minutes into the past, the state an admission outage leaves behind.
async fn late_schedule(pool: &PgPool, browser: &Browser, tenant: &support::Tenant) {
    let mut conn = pool.acquire().await.unwrap();
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

    let (status, policy) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({ "name": "nightly" })),
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
                "cron_expression": "0 * * * *",
                "time_zone": "UTC",
                "service_principal_id": service,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{schedule}");
    let schedule_id: ScheduleId = schedule["id"].as_str().unwrap().parse().unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let updated = sqlx::query(
        "UPDATE maintenance_schedule_state SET next_due_at = $3 \
         WHERE organization_id = $1 AND schedule_id = $2",
    )
    .bind(tenant.organization.as_uuid())
    .bind(schedule_id.as_uuid())
    .bind(Utc::now() - TimeDelta::minutes(10))
    .execute(&mut *conn)
    .await
    .unwrap();
    assert_eq!(updated.rows_affected(), 1, "schedule state exists");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn overview_answers_what_needs_attention_and_respects_grants(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "overview").await;
    let other = second_table(&mut conn, &tenant).await;
    // Created two days ago, ended now: the window follows the terminal
    // transition, so this job still counts today.
    let succeeded = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, succeeded, JobState::Succeeded).await;
    sqlx::query("UPDATE jobs SET created_at = now() - interval '2 days' WHERE id = $1")
        .bind(succeeded.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    // Reconciliation took two hours, but the conflict resolved just now: the
    // attention age follows the terminal transition.
    let replanned = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, replanned, JobState::ReplanRequired).await;
    sqlx::query(
        "UPDATE jobs SET publication_started_at = now() - interval '2 hours' WHERE id = $1",
    )
    .bind(replanned.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    // Six unresolved publications, more than the bounded list shows. The
    // first one has waited two hours.
    let mut unresolved = Vec::new();
    for index in 0..6 {
        let job = support::job(&mut conn, &other).await;
        support::force_job_state(&mut conn, job, JobState::CommitOutcomeUnknown).await;
        if index == 0 {
            sqlx::query(
                "UPDATE jobs SET publication_started_at = now() - interval '2 hours' \
                 WHERE id = $1",
            )
            .bind(job.as_uuid())
            .execute(&mut *conn)
            .await
            .unwrap();
        }
        unresolved.push(job);
    }
    let later = support::job(&mut conn, &other).await;
    support::force_job_state(&mut conn, later, JobState::Succeeded).await;
    drop(conn);

    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    late_schedule(&pool, &admin, &tenant).await;
    let (status, body) = admin.request("GET", "/api/v1/overview", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["jobs"]["by_state"],
        json!({
            "Succeeded": "2", "NoOp": "0", "Cancelled": "0",
            "ReplanRequired": "1", "Failed": "0",
        })
    );
    assert_eq!(body["jobs"]["ended"], "3");
    assert_eq!(body["jobs"]["conflict_rate_percent"], "33.3");

    let attention = &body["attention"];
    assert_eq!(attention["unresolved_publications"], "6");
    assert_eq!(attention["terminal_jobs"], "1");
    assert_eq!(attention["truncated"], true);
    let items = attention["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);
    assert_eq!(items[0]["job_id"], unresolved[0].to_string());
    assert_eq!(items[0]["reason"], "unresolved_publication");
    assert!(
        items[0]["age_seconds"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 7_200
    );
    assert!(
        attention["oldest_unresolved_age_seconds"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 7_200
    );

    assert_eq!(body["tables"]["total"], "2");
    assert_eq!(body["tables"]["never_sampled"], "2");
    assert_eq!(body["tables"]["latest_job_attention"], "1");

    assert_eq!(body["schedules"]["total"], "1");
    assert_eq!(body["schedules"]["enabled"], "1");
    assert_eq!(body["schedules"]["overdue"], "1");
    assert!(
        body["schedules"]["oldest_overdue_age_seconds"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            >= 600
    );
    assert_eq!(body["schedules"]["coalesced_last_day"], "0");
    assert_eq!(body["schedules"]["skipped_last_day"], "0");

    assert_eq!(body["workers"]["sessions"], "0");
    assert_eq!(body["workers"]["live_sessions"], "0");
    assert_eq!(body["workers"]["memory_bytes"], "0");
    assert_eq!(body["workers"]["running_attempts"], "0");

    // A viewer granted one table sees only its jobs and inventory, and no
    // schedule or worker section. Its conflict resolved just now, so the age
    // is the terminal transition, not the old publication start.
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    let (status, scoped) = viewer.request("GET", "/api/v1/overview", None, None).await;
    assert_eq!(status, StatusCode::OK, "{scoped}");
    assert_eq!(scoped["jobs"]["ended"], "2");
    assert_eq!(scoped["jobs"]["conflict_rate_percent"], "50.0");
    assert_eq!(scoped["attention"]["unresolved_publications"], "0");
    assert_eq!(scoped["attention"]["terminal_jobs"], "1");
    assert_eq!(scoped["attention"]["truncated"], false);
    let scoped_items = scoped["attention"]["items"].as_array().unwrap();
    assert_eq!(scoped_items.len(), 1);
    assert_eq!(scoped_items[0]["job_id"], replanned.to_string());
    assert!(
        scoped_items[0]["age_seconds"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            < 60,
        "the terminal transition, not the publication start"
    );
    assert_eq!(scoped["tables"]["total"], "1");
    assert_eq!(scoped["tables"]["never_sampled"], "1");
    assert_eq!(scoped["schedules"], Value::Null);
    assert_eq!(scoped["workers"], Value::Null);

    // No grant reaches nothing, and existence is not disclosed.
    let mut conn = pool.acquire().await.unwrap();
    let outsider = support::user(&mut conn, tenant.organization, "bob").await;
    let foreign = support::tenant(&mut conn, "overview-foreign").await;
    drop(conn);
    let anonymous = Browser::new(&pool, &tenant, outsider, "bob").await;
    let (status, empty) = anonymous
        .request("GET", "/api/v1/overview", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(empty["jobs"]["ended"], "0");
    assert_eq!(empty["attention"]["items"].as_array().unwrap().len(), 0);
    assert_eq!(empty["attention"]["unresolved_publications"], "0");
    assert_eq!(empty["tables"]["total"], "0");
    assert_eq!(empty["schedules"], Value::Null);

    let foreign_browser = Browser::new(&pool, &foreign, foreign.user, "alice").await;
    let (status, other_org) = foreign_browser
        .request("GET", "/api/v1/overview", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{other_org}");
    assert_eq!(other_org["jobs"]["ended"], "0");
    // Its administrator may read its own (empty) schedule and worker sections.
    assert_eq!(other_org["schedules"]["total"], "0");
    assert_eq!(other_org["workers"]["sessions"], "0");
}
