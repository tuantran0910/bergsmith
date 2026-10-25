//! Schedule occurrences and upcoming instants (BS-024.T2).
//!
//! Admission runs through the persistence repository; these tests cover the
//! resource fields the schedule page reads, the newest-first cursor, and the
//! mapping from a stored skip reason to the contract.

use super::*;

use bergsmith_common::ids::ScheduleId;
use bergsmith_persistence::occurrences;
use chrono::{DateTime, TimeDelta, TimeZone, Utc};

/// An hourly UTC schedule for the tenant's table, created through the API so
/// the answer is the resource the page reads.
async fn hourly_schedule(
    browser: &Browser,
    tenant: &support::Tenant,
    service: PrincipalId,
    window: Option<Value>,
) -> Value {
    let (status, policy) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({ "name": "nightly" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{policy}");
    let mut body = json!({
        "policy_id": policy["id"],
        "table_id": tenant.table,
        "cron_expression": "0 * * * *",
        "time_zone": "UTC",
        "service_principal_id": service,
    });
    if let Some(window) = window {
        body["window"] = window;
    }
    let (status, schedule) = browser
        .request("POST", "/api/v1/schedules", Some(body), None)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{schedule}");
    schedule
}

/// A service identity for the tenant's table and the leadership epoch.
async fn service_and_epoch(
    conn: &mut PgConnection,
    tenant: &support::Tenant,
) -> (PrincipalId, i64) {
    let service = support::service_principal(conn, tenant.organization, "scheduler").await;
    grants::create_grant(
        conn,
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
    let epoch = support::leader(conn).await;
    (service, epoch)
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_schedule_names_its_identity_and_lists_occurrences_newest_first(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (service, epoch) = service_and_epoch(&mut conn, &tenant).await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let schedule = hourly_schedule(&browser, &tenant, service, None).await;

    // The resource names the identity instead of returning only its identifier.
    assert_eq!(schedule["service_identity_name"], "scheduler");

    // The next five instants are DST-correct and strictly increasing.
    let upcoming = schedule["next_occurrences"].as_array().unwrap();
    assert_eq!(upcoming.len(), 5);
    let instants: Vec<DateTime<Utc>> = upcoming
        .iter()
        .map(|value| {
            DateTime::parse_from_rfc3339(value.as_str().unwrap())
                .unwrap()
                .with_timezone(&Utc)
        })
        .collect();
    assert!(instants[0] > Utc::now());
    for pair in instants.windows(2) {
        assert_eq!(pair[1] - pair[0], TimeDelta::hours(1));
    }

    // Admit one run, then let the job that owns the table coalesce the rest.
    let schedule_id = schedule["id"]
        .as_str()
        .unwrap()
        .parse::<ScheduleId>()
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let start = Utc.with_ymd_and_hms(2031, 1, 1, 0, 0, 0).single().unwrap();
    let admitted = occurrences::admit_at(&mut conn, schedule_id, start, epoch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(admitted.state.as_str(), "Admitted");
    for hour in 1..=2 {
        occurrences::admit_at(
            &mut conn,
            schedule_id,
            start + TimeDelta::hours(hour),
            epoch,
        )
        .await
        .unwrap()
        .unwrap();
    }
    drop(conn);

    let (status, page) = browser
        .request(
            "GET",
            &format!("/api/v1/schedules/{schedule_id}/occurrences?limit=2"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    // Newest first, and a coalesced row names the job that already owns work.
    assert!(items[0]["scheduled_for"].as_str() > items[1]["scheduled_for"].as_str());
    assert_eq!(items[0]["state"], "Coalesced");
    assert!(items[0]["job_id"].is_string());
    assert!(items[0]["reason"].is_string());

    // The cursor continues from the last instant read.
    let cursor = page["next_cursor"].as_str().unwrap();
    let (status, rest) = browser
        .request(
            "GET",
            &format!("/api/v1/schedules/{schedule_id}/occurrences?limit=2&cursor={cursor}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rest}");
    let rest = rest["items"].as_array().unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0]["state"], "Admitted");
    assert_eq!(rest[0]["reason"], Value::Null);

    // A cursor is bound to its organization, caller, and schedule; reusing it
    // against another schedule is invalid rather than silently misplaced.
    let (status, reused) = browser
        .request(
            "GET",
            &format!(
                "/api/v1/schedules/{}/occurrences?cursor={cursor}",
                ScheduleId::generate()
            ),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{reused}");
    assert_eq!(reused["reason"], "invalid_request");

    // An unknown schedule is a 404, not an empty history.
    let (status, _) = browser
        .request(
            "GET",
            &format!("/api/v1/schedules/{}/occurrences", ScheduleId::generate()),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_skipped_evaluation_reports_its_stable_reason(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let (service, epoch) = service_and_epoch(&mut conn, &tenant).await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    // The window opens at 02:00 local time, so an evaluation at midnight is
    // outside it.
    let schedule = hourly_schedule(
        &browser,
        &tenant,
        service,
        Some(json!({ "start_minute": 120, "end_minute": 180 })),
    )
    .await;

    let schedule_id = schedule["id"]
        .as_str()
        .unwrap()
        .parse::<ScheduleId>()
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let at = Utc.with_ymd_and_hms(2031, 1, 1, 0, 0, 0).single().unwrap();
    let skipped = occurrences::admit_at(&mut conn, schedule_id, at, epoch)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(skipped.state.as_str(), "Skipped");
    drop(conn);

    let (status, page) = browser
        .request(
            "GET",
            &format!("/api/v1/schedules/{schedule_id}/occurrences"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["state"], "Skipped");
    assert_eq!(items[0]["reason"], "OutsideWindow");
    assert_eq!(items[0]["job_id"], Value::Null);
    assert_eq!(page["next_cursor"], Value::Null);
}
