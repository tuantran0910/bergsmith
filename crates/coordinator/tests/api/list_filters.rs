//! Filtered lists, name search, and facet counts (BS-033.T4).
//!
//! Each list narrows by its filters before pagination, binds its cursor to
//! them, and counts its facets through the same grant predicate. Two
//! principals on disjoint grants read the same ledger, so neither a match nor
//! a count can reveal what the other sees.

use super::*;

use bergsmith_common::audit::{ActorKind, AuditAction, AuditOutcome};
use bergsmith_common::ids::{JobId, TableId};
use bergsmith_common::job::JobOperation;
use bergsmith_persistence::audit::{self, NewAuditEvent};
use bergsmith_persistence::jobs::{self, NewJob};

/// A table `db.<name>` on `connection` in the tenant's organization.
async fn table_on(
    conn: &mut PgConnection,
    tenant: &support::Tenant,
    connection: CatalogConnectionId,
    name: &str,
) -> TableId {
    tables::resolve_table(
        conn,
        tenant.organization,
        &support::alias(connection, name),
        Uuid::now_v7(),
    )
    .await
    .unwrap()
    .id
}

/// A job of `operation` on `table`, moved straight to `state`.
async fn job_on(
    conn: &mut PgConnection,
    tenant: &support::Tenant,
    table: TableId,
    operation: JobOperation,
    state: JobState,
) -> String {
    let id = JobId::generate();
    jobs::create_job(
        conn,
        &NewJob {
            id,
            organization_id: tenant.organization,
            table_id: table,
            operation,
            retry_of: None,
            created_by: tenant.user,
        },
    )
    .await
    .unwrap();
    support::force_job_state(conn, id, state).await;
    id.to_string()
}

/// The `id` of every item of a page, in order.
fn ids(body: &Value) -> Vec<String> {
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect()
}

/// One facet dimension as `(value, count)` pairs, in order.
fn counts(dimension: &Value) -> Vec<(String, i64)> {
    dimension
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["value"].as_str().unwrap().to_owned(),
                entry["count"].as_i64().unwrap(),
            )
        })
        .collect()
}

fn pairs(expected: &[(&str, i64)]) -> Vec<(String, i64)> {
    expected
        .iter()
        .map(|(value, count)| ((*value).to_owned(), *count))
        .collect()
}

async fn ok(browser: &Browser, uri: &str) -> Value {
    let (status, body) = browser.request("GET", uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    body
}

/// Four jobs on two tables: `events` succeeded and failed, `orders` failed
/// and running.
struct Jobs {
    tenant: support::Tenant,
    succeeded: String,
    expired: String,
    failed: String,
    running: String,
}

async fn four_jobs(pool: &PgPool) -> Jobs {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let orders = table_on(&mut conn, &tenant, tenant.connection, "orders").await;
    let succeeded = job_on(
        &mut conn,
        &tenant,
        tenant.table,
        JobOperation::Compact,
        JobState::Succeeded,
    )
    .await;
    let expired = job_on(
        &mut conn,
        &tenant,
        tenant.table,
        JobOperation::ExpireSnapshots,
        JobState::Failed,
    )
    .await;
    let failed = job_on(
        &mut conn,
        &tenant,
        orders,
        JobOperation::Compact,
        JobState::Failed,
    )
    .await;
    let running = job_on(
        &mut conn,
        &tenant,
        orders,
        JobOperation::Compact,
        JobState::Running,
    )
    .await;
    Jobs {
        tenant,
        succeeded,
        expired,
        failed,
        running,
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn jobs_filter_by_state_operation_table_requester_and_name(pool: PgPool) {
    let jobs = four_jobs(&pool).await;
    let tenant = &jobs.tenant;
    let browser = Browser::new(&pool, tenant, tenant.user, "alice").await;

    let failed = ok(&browser, "/api/v1/jobs?state=Failed").await;
    assert_eq!(ids(&failed), [jobs.expired.as_str(), jobs.failed.as_str()]);
    // Values of one filter combine with OR; filters combine with AND.
    let compact = ok(
        &browser,
        "/api/v1/jobs?state=Failed,Running&operation=compact",
    )
    .await;
    assert_eq!(ids(&compact), [jobs.failed.as_str(), jobs.running.as_str()]);
    let events = ok(&browser, &format!("/api/v1/jobs?table={}", tenant.table)).await;
    assert_eq!(
        ids(&events),
        [jobs.succeeded.as_str(), jobs.expired.as_str()]
    );
    let mine = ok(
        &browser,
        &format!("/api/v1/jobs?created_by={}", tenant.user),
    )
    .await;
    assert_eq!(ids(&mine).len(), 4);
    let other = ok(
        &browser,
        &format!("/api/v1/jobs?created_by={}", Uuid::now_v7()),
    )
    .await;
    assert!(ids(&other).is_empty());

    // The name search is case-insensitive over `namespace.name`.
    let named = ok(&browser, "/api/v1/jobs?q=DB.ORD").await;
    assert_eq!(ids(&named), [jobs.failed.as_str(), jobs.running.as_str()]);
    // A wildcard matches itself, never every name.
    for wildcard in ["%25", "_"] {
        let literal = ok(&browser, &format!("/api/v1/jobs?q={wildcard}")).await;
        assert!(ids(&literal).is_empty(), "{wildcard}");
    }
    let catalog = ok(
        &browser,
        &format!("/api/v1/jobs?catalog={}", tenant.connection),
    )
    .await;
    assert_eq!(ids(&catalog).len(), 4);

    let newest = ok(&browser, "/api/v1/jobs?order=newest").await;
    assert_eq!(
        ids(&newest),
        [
            jobs.running.as_str(),
            jobs.failed.as_str(),
            jobs.expired.as_str(),
            jobs.succeeded.as_str()
        ]
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_filtered_cursor_resumes_only_under_its_filter(pool: PgPool) {
    let jobs = four_jobs(&pool).await;
    let tenant = &jobs.tenant;
    let browser = Browser::new(&pool, tenant, tenant.user, "alice").await;

    let first = ok(&browser, "/api/v1/jobs?state=Failed&limit=1").await;
    assert_eq!(ids(&first), [jobs.expired.as_str()]);
    let cursor = first["next_cursor"].as_str().unwrap();
    let second = ok(
        &browser,
        &format!("/api/v1/jobs?state=Failed&limit=1&cursor={cursor}"),
    )
    .await;
    assert_eq!(ids(&second), [jobs.failed.as_str()]);
    assert!(second["next_cursor"].is_null());
    for other in [
        "state=Running",
        "state=Failed&order=newest",
        "state=Failed&q=events",
        "",
    ] {
        let (status, _) = browser
            .request(
                "GET",
                &format!("/api/v1/jobs?{other}&limit=1&cursor={cursor}"),
                None,
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{other}");
    }

    // The newest-first order pages downward.
    let first = ok(&browser, "/api/v1/jobs?order=newest&limit=2").await;
    assert_eq!(ids(&first), [jobs.running.as_str(), jobs.failed.as_str()]);
    let cursor = first["next_cursor"].as_str().unwrap();
    let second = ok(
        &browser,
        &format!("/api/v1/jobs?order=newest&limit=2&cursor={cursor}"),
    )
    .await;
    assert_eq!(
        ids(&second),
        [jobs.expired.as_str(), jobs.succeeded.as_str()]
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn job_facets_count_each_dimension_without_its_own_filter(pool: PgPool) {
    let jobs = four_jobs(&pool).await;
    let tenant = &jobs.tenant;
    let browser = Browser::new(&pool, tenant, tenant.user, "alice").await;

    let all = ok(&browser, "/api/v1/jobs/facets").await;
    // States in lifecycle order, operations in their declared order.
    assert_eq!(
        counts(&all["state"]),
        pairs(&[("Running", 1), ("Succeeded", 1), ("Failed", 2)])
    );
    assert_eq!(
        counts(&all["operation"]),
        pairs(&[("compact", 3), ("expire_snapshots", 1)])
    );

    let failed = ok(&browser, "/api/v1/jobs/facets?state=Failed").await;
    assert_eq!(failed["state"], all["state"]);
    assert_eq!(
        counts(&failed["operation"]),
        pairs(&[("compact", 1), ("expire_snapshots", 1)])
    );

    let orders = ok(&browser, "/api/v1/jobs/facets?operation=compact&q=orders").await;
    assert_eq!(
        counts(&orders["state"]),
        pairs(&[("Running", 1), ("Failed", 1)])
    );
    assert_eq!(counts(&orders["operation"]), pairs(&[("compact", 2)]));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn filters_and_facets_never_reveal_what_a_grant_hides(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let hidden = support::connection(&mut conn, tenant.organization, "hidden").await;
    let secret = table_on(&mut conn, &tenant, hidden, "secret").await;
    // `events` is also named `db.classified` in the hidden catalog.
    let table_uuid: Uuid =
        sqlx::query_scalar("SELECT table_uuid FROM table_identities WHERE id = $1")
            .bind(tenant.table.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(hidden, "classified"),
        table_uuid,
    )
    .await
    .unwrap();
    let visible = job_on(
        &mut conn,
        &tenant,
        tenant.table,
        JobOperation::Compact,
        JobState::Failed,
    )
    .await;
    job_on(
        &mut conn,
        &tenant,
        secret,
        JobOperation::ExpireSnapshots,
        JobState::Failed,
    )
    .await;
    drop(conn);

    let (main_viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::connection(tenant.connection),
    )
    .await;
    let jobs = ok(&main_viewer, "/api/v1/jobs/facets").await;
    assert_eq!(counts(&jobs["state"]), pairs(&[("Failed", 1)]));
    assert_eq!(counts(&jobs["operation"]), pairs(&[("compact", 1)]));
    // A name or a catalog the viewer cannot see matches nothing, even for a
    // table the viewer can see by another name.
    for query in [
        "q=classified".to_owned(),
        "q=secret".to_owned(),
        format!("catalog={hidden}"),
    ] {
        let page = ok(&main_viewer, &format!("/api/v1/jobs?{query}")).await;
        assert!(ids(&page).is_empty(), "jobs {query}");
        let page = ok(&main_viewer, &format!("/api/v1/tables?{query}")).await;
        assert!(ids(&page).is_empty(), "tables {query}");
    }
    let named = ok(&main_viewer, "/api/v1/jobs?q=events").await;
    assert_eq!(ids(&named), [visible.as_str()]);
    let tables = ok(&main_viewer, "/api/v1/tables/facets").await;
    assert_eq!(
        counts(&tables["catalog"]),
        pairs(&[(&tenant.connection.to_string(), 1)])
    );
    assert_eq!(counts(&tables["health"]), pairs(&[("unassessed", 1)]));
    let catalogs = ok(&main_viewer, "/api/v1/catalogs/facets").await;
    assert_eq!(counts(&catalogs["enabled"]), pairs(&[("true", 1)]));
    let catalogs = ok(&main_viewer, "/api/v1/catalogs?q=hidden").await;
    assert!(ids(&catalogs).is_empty());

    // The other grant sees both tables through the hidden catalog, and only
    // that catalog.
    let (hidden_viewer, _) =
        scoped(&pool, &tenant, Role::Viewer, GrantScope::connection(hidden)).await;
    let jobs = ok(&hidden_viewer, "/api/v1/jobs/facets").await;
    assert_eq!(counts(&jobs["state"]), pairs(&[("Failed", 2)]));
    let tables = ok(&hidden_viewer, "/api/v1/tables/facets").await;
    assert_eq!(
        counts(&tables["catalog"]),
        pairs(&[(&hidden.to_string(), 2)])
    );
    let named = ok(&hidden_viewer, "/api/v1/tables?q=classified").await;
    assert_eq!(ids(&named), [tenant.table.to_string()]);
    let named = ok(&hidden_viewer, "/api/v1/jobs?q=db.events").await;
    assert!(ids(&named).is_empty());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn tables_filter_by_catalog_and_name_and_count_health(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let lake = support::connection(&mut conn, tenant.organization, "lake").await;
    let orders = table_on(&mut conn, &tenant, lake, "orders").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let in_lake = ok(&browser, &format!("/api/v1/tables?catalog={lake}")).await;
    assert_eq!(ids(&in_lake), [orders.to_string()]);
    let named = ok(&browser, "/api/v1/tables?q=EVENTS").await;
    assert_eq!(ids(&named), [tenant.table.to_string()]);
    // The health scan applies the same filters.
    let scanned = ok(&browser, "/api/v1/tables?q=events&health=unassessed").await;
    assert_eq!(ids(&scanned), [tenant.table.to_string()]);
    let scanned = ok(
        &browser,
        &format!("/api/v1/tables?catalog={lake}&sort=health"),
    )
    .await;
    assert_eq!(ids(&scanned), [orders.to_string()]);

    let mut both = vec![(tenant.connection.to_string(), 1), (lake.to_string(), 1)];
    both.sort();
    let all = ok(&browser, "/api/v1/tables/facets").await;
    assert_eq!(counts(&all["health"]), pairs(&[("unassessed", 2)]));
    assert_eq!(counts(&all["catalog"]), both);
    let in_lake = ok(&browser, &format!("/api/v1/tables/facets?catalog={lake}")).await;
    assert_eq!(counts(&in_lake["health"]), pairs(&[("unassessed", 1)]));
    assert_eq!(counts(&in_lake["catalog"]), both);
    let critical = ok(&browser, "/api/v1/tables/facets?health=critical").await;
    assert_eq!(counts(&critical["health"]), pairs(&[("unassessed", 2)]));
    assert!(counts(&critical["catalog"]).is_empty());
    for query in ["limit=1", "cursor=x", "sort=health"] {
        let (status, _) = browser
            .request("GET", &format!("/api/v1/tables/facets?{query}"), None, None)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn catalogs_filter_by_name_and_enabled(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let lake = support::connection(&mut conn, tenant.organization, "lake").await;
    sqlx::query("UPDATE catalog_connections SET enabled = false WHERE id = $1")
        .bind(lake.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let named = ok(&browser, "/api/v1/catalogs?q=LA").await;
    assert_eq!(ids(&named), [lake.to_string()]);
    let disabled = ok(&browser, "/api/v1/catalogs?enabled=false").await;
    assert_eq!(ids(&disabled), [lake.to_string()]);
    let enabled = ok(&browser, "/api/v1/catalogs?enabled=true").await;
    assert_eq!(ids(&enabled), [tenant.connection.to_string()]);
    let literal = ok(&browser, "/api/v1/catalogs?q=_").await;
    assert!(ids(&literal).is_empty());

    let all = ok(&browser, "/api/v1/catalogs/facets").await;
    assert_eq!(counts(&all["enabled"]), pairs(&[("true", 1), ("false", 1)]));
    let main = ok(&browser, "/api/v1/catalogs/facets?q=main").await;
    assert_eq!(counts(&main["enabled"]), pairs(&[("true", 1)]));
    let own = ok(&browser, "/api/v1/catalogs/facets?enabled=false").await;
    assert_eq!(own["enabled"], all["enabled"]);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn audit_filters_order_and_facets(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    for (actor, action, resource, outcome) in [
        (
            Some(tenant.user),
            AuditAction::JobAdmitted,
            "job",
            AuditOutcome::Allowed,
        ),
        (
            Some(tenant.user),
            AuditAction::ApiPermissionDenied,
            "table",
            AuditOutcome::Denied,
        ),
        (
            None,
            AuditAction::GrantCreated,
            "grant",
            AuditOutcome::Allowed,
        ),
    ] {
        audit::record(
            &mut conn,
            &NewAuditEvent {
                organization_id: Some(tenant.organization),
                actor_principal_id: actor,
                actor_kind: if actor.is_some() {
                    ActorKind::User
                } else {
                    ActorKind::System
                },
                action: action.as_str(),
                resource_type: resource,
                resource_id: None,
                outcome,
                detail: json!({}),
            },
        )
        .await
        .unwrap();
    }
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let denied = ok(&browser, "/api/v1/audit?outcome=Denied").await;
    let actions: Vec<_> = denied["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["api.permission_denied"]);
    let newest = ok(
        &browser,
        "/api/v1/audit?action=job.admitted,grant.created&order=newest",
    )
    .await;
    let actions: Vec<_> = newest["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["grant.created", "job.admitted"]);
    let by_alice = ok(&browser, "/api/v1/audit?q=ALI&resource_type=job,table").await;
    assert_eq!(by_alice["items"].as_array().unwrap().len(), 2);
    let by_actor = ok(
        &browser,
        &format!("/api/v1/audit?actor={}&outcome=Allowed", tenant.user),
    )
    .await;
    let actions: Vec<_> = by_actor["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["action"].as_str().unwrap())
        .collect();
    assert!(actions.contains(&"job.admitted"), "{by_actor}");
    assert!(!actions.contains(&"grant.created"), "{by_actor}");

    let facets = ok(&browser, "/api/v1/audit/facets?outcome=Allowed").await;
    let outcomes = counts(&facets["outcome"]);
    assert!(outcomes.contains(&("Denied".to_owned(), 1)), "{facets}");
    assert!(
        !counts(&facets["action"])
            .iter()
            .any(|(action, _)| action == "api.permission_denied")
    );
    assert!(counts(&facets["resource_type"]).contains(&("grant".to_owned(), 1)));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn principals_filter_by_activity_and_name(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let bob = support::user(&mut conn, tenant.organization, "bob").await;
    let robot = support::service_principal(&mut conn, tenant.organization, "robot").await;
    sqlx::query("UPDATE principals SET deactivated_at = now() WHERE id = $1")
        .bind(bob.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let inactive = ok(&browser, "/api/v1/principals?active=false").await;
    assert_eq!(ids(&inactive), [bob.to_string()]);
    let named = ok(&browser, "/api/v1/principals?q=ROB").await;
    assert_eq!(ids(&named), [robot.to_string()]);
    let people = ok(&browser, "/api/v1/principals?kind=User&active=true").await;
    assert_eq!(ids(&people), [tenant.user.to_string()]);

    let all = ok(&browser, "/api/v1/principals/facets").await;
    assert_eq!(counts(&all["kind"]), pairs(&[("User", 2), ("Service", 1)]));
    assert_eq!(counts(&all["active"]), pairs(&[("true", 2), ("false", 1)]));
    let services = ok(&browser, "/api/v1/principals/facets?kind=Service").await;
    assert_eq!(services["kind"], all["kind"]);
    assert_eq!(counts(&services["active"]), pairs(&[("true", 1)]));

    // Counting principals is access administration, like listing them.
    let (viewer, _) = scoped(&pool, &tenant, Role::Viewer, GrantScope::organization()).await;
    let (status, _) = viewer
        .request("GET", "/api/v1/principals/facets", None, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = viewer
        .request("GET", "/api/v1/audit/facets", None, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn malformed_filters_are_rejected_not_ignored(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let too_long = "a".repeat(129);
    let too_many = (0..17).map(|_| "Failed").collect::<Vec<_>>().join(",");
    let every_state = JobState::ALL
        .iter()
        .map(|state| state.as_str())
        .collect::<Vec<_>>()
        .join(",");
    for uri in [
        "/api/v1/jobs?state=Bogus".to_owned(),
        "/api/v1/jobs?state=".to_owned(),
        "/api/v1/jobs?state=Failed,".to_owned(),
        // The API name of an operation, not its stored name.
        "/api/v1/jobs?operation=Compact".to_owned(),
        "/api/v1/jobs?q=".to_owned(),
        "/api/v1/jobs?q=%20%20".to_owned(),
        format!("/api/v1/jobs?q={too_long}"),
        "/api/v1/jobs?q=a%0Ab".to_owned(),
        "/api/v1/jobs?table=nope".to_owned(),
        "/api/v1/jobs?order=up".to_owned(),
        "/api/v1/jobs?unknown=1".to_owned(),
        "/api/v1/jobs/facets?limit=1".to_owned(),
        "/api/v1/jobs/facets?order=newest".to_owned(),
        "/api/v1/catalogs?enabled=yes".to_owned(),
        "/api/v1/catalogs/facets?cursor=x".to_owned(),
        "/api/v1/principals?active=1".to_owned(),
        "/api/v1/audit?outcome=allowed".to_owned(),
        "/api/v1/audit?action=job.unknown".to_owned(),
        "/api/v1/audit?resource_type=Table".to_owned(),
        "/api/v1/audit/facets?order=newest".to_owned(),
    ] {
        let (status, body) = browser.request("GET", &uri, None, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
    }
    // Repeating one value is not more values, and every state fits.
    ok(&browser, &format!("/api/v1/jobs?state={too_many}")).await;
    ok(&browser, &format!("/api/v1/jobs?state={every_state}")).await;
    ok(&browser, &format!("/api/v1/jobs?q={}", "é".repeat(128))).await;
}

/// Each list filter reads only its matching rows. A ledger of 5,000 jobs
/// and audit records, analyzed, where each filtered value is rare: the
/// executed plan must not scan the table or discard the organization's other
/// rows one by one, which is what a filter without an index range does.
#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn every_filter_has_an_index_range_in_list_order(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let rare_table = table_on(&mut conn, &tenant, tenant.connection, "rare").await;
    let rare_user = support::user(&mut conn, tenant.organization, "rare").await;
    // One row in a hundred carries each rare value.
    sqlx::query(
        "INSERT INTO jobs (id, organization_id, table_identity_id, state, operation, created_by)
         SELECT gen_random_uuid(), $1,
                CASE WHEN g % 100 = 1 THEN $3 ELSE $2 END,
                CASE WHEN g % 100 = 2 THEN 'Failed' ELSE 'Succeeded' END,
                CASE WHEN g % 100 = 3 THEN 'ExpireSnapshots' ELSE 'Compact' END,
                CASE WHEN g % 100 = 4 THEN $5 ELSE $4 END
         FROM generate_series(1, 5000) g",
    )
    .bind(tenant.organization.as_uuid())
    .bind(tenant.table.as_uuid())
    .bind(rare_table.as_uuid())
    .bind(tenant.user.as_uuid())
    .bind(rare_user.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO audit_events
             (organization_id, actor_principal_id, actor_kind, action, resource_type, outcome)
         SELECT $1, CASE WHEN g % 100 = 1 THEN $2 END,
                CASE WHEN g % 100 = 1 THEN 'User' ELSE 'System' END,
                CASE WHEN g % 100 = 2 THEN 'grant.created' ELSE 'job.admitted' END,
                'job',
                CASE WHEN g % 100 = 3 THEN 'Denied' ELSE 'Allowed' END
         FROM generate_series(1, 5000) g",
    )
    .bind(tenant.organization.as_uuid())
    .bind(rare_user.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("ANALYZE jobs, audit_events")
        .execute(&mut *conn)
        .await
        .unwrap();
    let rare_table = rare_table.to_string();
    let rare_user = rare_user.to_string();
    for (sql, value) in [
        (
            "SELECT id FROM jobs WHERE organization_id = $1 AND state = $2 ORDER BY id DESC LIMIT 51",
            "Failed",
        ),
        (
            "SELECT id FROM jobs WHERE organization_id = $1 AND operation = $2 ORDER BY id LIMIT 51",
            "ExpireSnapshots",
        ),
        (
            "SELECT id FROM jobs WHERE organization_id = $1 AND table_identity_id = $2::uuid ORDER BY id LIMIT 51",
            rare_table.as_str(),
        ),
        (
            "SELECT id FROM jobs WHERE organization_id = $1 AND created_by = $2::uuid ORDER BY id DESC LIMIT 51",
            rare_user.as_str(),
        ),
        (
            "SELECT id FROM audit_events WHERE organization_id = $1 AND outcome = $2 ORDER BY id DESC LIMIT 51",
            "Denied",
        ),
        (
            "SELECT id FROM audit_events WHERE organization_id = $1 AND action = $2 ORDER BY id LIMIT 51",
            "grant.created",
        ),
        (
            "SELECT id FROM audit_events WHERE organization_id = $1 AND actor_principal_id = $2::uuid ORDER BY id DESC LIMIT 51",
            rare_user.as_str(),
        ),
    ] {
        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (ANALYZE, TIMING off, SUMMARY off) {sql}"
        )))
        .bind(tenant.organization.as_uuid())
        .bind(value)
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        let plan = plan.join("\n");
        assert!(!plan.contains("Seq Scan"), "{sql}\n{plan}");
        let discarded: u64 = plan
            .lines()
            .filter_map(|line| line.trim().strip_prefix("Rows Removed by Filter: "))
            .map(|rows| rows.parse::<u64>().unwrap())
            .sum();
        // Fifty rows match; an unindexed filter discards about 4,950.
        assert!(discarded < 100, "{sql}\n{plan}");
    }
}
