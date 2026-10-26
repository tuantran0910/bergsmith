//! Browser resource contracts against the real identity and execution ledger.
#![expect(clippy::unwrap_used, reason = "route fixtures fail on setup errors")]

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

#[path = "api/catalogs.rs"]
mod catalog_management;

#[path = "api/discovery.rs"]
mod catalog_discovery;

#[path = "api/grants.rs"]
mod grants_api;

#[path = "api/job_statistics.rs"]
mod job_statistics;

#[path = "api/overview.rs"]
mod overview;

#[path = "api/principals.rs"]
mod principals;

#[path = "api/schedule_occurrences.rs"]
mod schedule_occurrences;

#[path = "api/service_identities.rs"]
mod service_identities;

#[path = "api/table_definition.rs"]
mod table_definition;

#[path = "api/table_maintenance.rs"]
mod table_maintenance;

#[path = "api/table_statistics.rs"]
mod table_statistics;

#[path = "api/table_snapshots.rs"]
mod table_snapshots;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bergsmith_common::ids::{PolicyId, PrincipalId, RoleGrantId};
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_coordinator::api::{self, ApiState, EventStreamLimits, JobEventHub};
use bergsmith_coordinator::checks::{CapabilityCheckLimits, CapabilityChecker};
use bergsmith_coordinator::discovery::{CatalogDiscovery, DiscoveryLimits};
use bergsmith_coordinator::identity::{
    CSRF_HEADER,
    session::{self, SessionConfig},
};
use bergsmith_coordinator::preview::{CatalogPreviewer, PreviewLimits, PreviewProfiles};
use bergsmith_coordinator::runtime::catalogs::RuntimeCatalogs;
use bergsmith_coordinator::runtime::config::RuntimeProfiles;
use bergsmith_maintenance::PlannerConfig;
use bergsmith_persistence::Database;
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_storage::{GcsProfile, S3Profile, StorageProfile, StorageProfiles};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use tower::ServiceExt;

/// A previewer over the local filesystem, which is where fixtures live.
fn previewer() -> CatalogPreviewer {
    CatalogPreviewer::new(
        PreviewProfiles::new(StorageProfiles::new([StorageProfile::LocalFs])),
        PlannerConfig::default(),
    )
    .unwrap()
}

fn event_hub() -> JobEventHub {
    JobEventHub::new(EventStreamLimits::default()).unwrap()
}

/// Discovery over the runtime profile set, which route tests replace with a
/// mock catalog.
fn discovery(pool: &PgPool) -> CatalogDiscovery {
    CatalogDiscovery::new(
        Arc::new(RuntimeCatalogs::new(
            Database::from_pool(pool.clone()),
            RuntimeProfiles {
                default: StorageProfiles::new([StorageProfile::LocalFs]),
                connections: BTreeMap::new(),
            },
        )),
        DiscoveryLimits::default(),
    )
}

#[derive(Clone)]
struct Browser {
    router: Router,
    cookie: String,
    csrf: String,
}

impl Browser {
    async fn new(
        pool: &PgPool,
        tenant: &support::Tenant,
        principal: PrincipalId,
        subject: &str,
    ) -> Self {
        Self::with_previews(pool, tenant, principal, subject, previewer()).await
    }

    async fn with_previews(
        pool: &PgPool,
        tenant: &support::Tenant,
        principal: PrincipalId,
        subject: &str,
        previews: CatalogPreviewer,
    ) -> Self {
        Self::with_state(pool, tenant, principal, subject, previews, event_hub()).await
    }

    async fn with_events(
        pool: &PgPool,
        tenant: &support::Tenant,
        principal: PrincipalId,
        subject: &str,
        events: JobEventHub,
    ) -> Self {
        Self::with_state(pool, tenant, principal, subject, previewer(), events).await
    }

    async fn with_state(
        pool: &PgPool,
        tenant: &support::Tenant,
        principal: PrincipalId,
        subject: &str,
        previews: CatalogPreviewer,
        events: JobEventHub,
    ) -> Self {
        let config = SessionConfig {
            secure: false,
            ..SessionConfig::default()
        };
        let mut conn = pool.acquire().await.unwrap();
        let issued = session::issue(
            &mut conn,
            &config,
            tenant.organization,
            principal,
            "https://idp.example",
            subject,
        )
        .await
        .unwrap();
        Self {
            router: api::router(ApiState {
                database: Database::from_pool(pool.clone()),
                sessions: config.clone(),
                previews,
                events,
                // The production bound is ten seconds. A test machine under
                // load must not fail a route test for that, so the fixture
                // waits longer; the bound itself has its own test.
                checks: CapabilityChecker::new(CapabilityCheckLimits {
                    timeout: Duration::from_secs(60),
                    connect_timeout: Duration::from_secs(30),
                    request_timeout: Duration::from_secs(60),
                    ..CapabilityCheckLimits::default()
                }),
                discovery: discovery(pool),
            }),
            cookie: format!("{}={}", config.cookie_name, issued.token.expose()),
            csrf: issued.csrf.expose().into(),
        }
    }

    async fn request(
        &self,
        method: &str,
        uri: &str,
        body: Option<Value>,
        key: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", &self.cookie)
            .header(CSRF_HEADER, &self.csrf);
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        let body = match body {
            Some(body) => {
                request = request.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        assert_eq!(response.headers()["cache-control"], "no-store");
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        check_contract(method, uri, status, &body);
        (status, body)
    }
}

fn router_with(pool: &PgPool, config: SessionConfig) -> Router {
    api::router(ApiState {
        database: Database::from_pool(pool.clone()),
        sessions: config,
        previews: previewer(),
        events: event_hub(),
        checks: CapabilityChecker::default(),
        discovery: discovery(pool),
    })
}

async fn raw(router: &Router, method: &str, uri: &str) -> (StatusCode, HeaderMap, Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, headers, body)
}

// Check the exported contract against real responses without duplicating DTOs
// in the test. This checks the schema features used by this API, not arbitrary
// OpenAPI documents.
fn check_contract(method: &str, uri: &str, status: StatusCode, body: &Value) {
    let document: Value = serde_json::from_str(include_str!("../src/api/openapi.json")).unwrap();
    let path = uri.split('?').next().unwrap();
    // Normalize every UUID segment, including one before a sub-resource verb.
    let segments = path.split('/').collect::<Vec<_>>();
    let template = segments
        .iter()
        .enumerate()
        .map(|(index, segment)| {
            if Uuid::parse_str(segment).is_ok() {
                // A token id hangs below its `tokens` collection; every other
                // UUID is the resource named `{id}`.
                if segments.get(index.wrapping_sub(1)) == Some(&"tokens") {
                    "{token}"
                } else {
                    "{id}"
                }
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    if method == "POST" {
        assert!(
            document["paths"][&template]["post"]["parameters"]
                .as_array()
                .unwrap()
                .iter()
                .any(|parameter| parameter["name"] == CSRF_HEADER)
        );
    }
    let response = &document["paths"][template][method.to_lowercase()]["responses"]
        [status.as_u16().to_string()];
    assert!(
        !response.is_null(),
        "undocumented response: {method} {uri} {status}"
    );
    check_schema(
        &document,
        &response["content"]["application/json"]["schema"],
        body,
    );
}

#[expect(
    clippy::panic,
    reason = "a schema type unsupported by the contract check must fail the test"
)]
fn check_schema(document: &Value, schema: &Value, body: &Value) {
    if let Some(reference) = schema["$ref"].as_str() {
        return check_schema(
            document,
            document
                .pointer(reference.strip_prefix('#').unwrap())
                .unwrap(),
            body,
        );
    }
    if let Some(choices) = schema["anyOf"].as_array() {
        if body.is_null() {
            assert!(choices.iter().any(|choice| choice["type"] == "null"));
            return;
        }
        return check_schema(document, &choices[0], body);
    }
    match schema["type"].as_str().unwrap() {
        "object" => {
            let fields = body.as_object().unwrap();
            if let Some(required) = schema["required"].as_array() {
                for name in required {
                    assert!(fields.contains_key(name.as_str().unwrap()));
                }
            }
            if schema["additionalProperties"] == false {
                assert!(
                    fields
                        .keys()
                        .all(|name| !schema["properties"][name].is_null())
                );
            }
            for (name, value) in fields {
                if !schema["properties"][name].is_null() {
                    check_schema(document, &schema["properties"][name], value);
                }
            }
        }
        "array" => {
            for value in body.as_array().unwrap() {
                check_schema(document, &schema["items"], value);
            }
        }
        "string" => {
            let value = body.as_str().unwrap();
            match schema["format"].as_str() {
                Some("uuid") => {
                    Uuid::parse_str(value).unwrap();
                }
                Some("date-time") => {
                    chrono::DateTime::parse_from_rfc3339(value).unwrap();
                }
                _ => {}
            }
        }
        "integer" => {
            assert!(body.is_i64() || body.is_u64());
        }
        "boolean" => {
            assert!(body.is_boolean());
        }
        other => panic!("unhandled contract type {other}"),
    }
    if let Some(choices) = schema["enum"].as_array() {
        assert!(choices.contains(body), "undocumented value {body}");
    }
}

async fn scoped(
    pool: &PgPool,
    tenant: &support::Tenant,
    role: Role,
    scope: GrantScope,
) -> (Browser, RoleGrantId) {
    let mut conn = pool.acquire().await.unwrap();
    let subject = RoleGrantId::generate().to_string();
    let principal = support::user(&mut conn, tenant.organization, &subject).await;
    let id = RoleGrantId::generate();
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id,
            organization_id: tenant.organization,
            principal_id: principal,
            role,
            scope,
            granted_by: None,
        },
    )
    .await
    .unwrap();
    drop(conn);
    (Browser::new(pool, tenant, principal, &subject).await, id)
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn reads_filter_before_pagination_and_hide_foreign_resources(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other = support::tenant(&mut conn, "other").await;
    let hidden = support::connection(&mut conn, tenant.organization, "hidden").await;
    let table_uuid: Uuid =
        sqlx::query_scalar("SELECT table_uuid FROM table_identities WHERE id = $1")
            .bind(tenant.table.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(hidden, "hidden-alias"),
        table_uuid,
    )
    .await
    .unwrap();
    let job = support::job(&mut conn, &tenant).await;
    let foreign_job = support::job(&mut conn, &other).await;
    drop(conn);
    let (browser, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::connection(tenant.connection),
    )
    .await;
    for (resource, id, foreign) in [
        (
            "catalogs",
            tenant.connection.to_string(),
            hidden.to_string(),
        ),
        ("tables", tenant.table.to_string(), other.table.to_string()),
        ("jobs", job.to_string(), foreign_job.to_string()),
    ] {
        let (status, body) = browser
            .request("GET", &format!("/api/v1/{resource}?limit=1"), None, None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        assert_eq!(body["items"][0]["id"], id);
        assert!(body["next_cursor"].is_null());
        let (status, mut detail) = browser
            .request("GET", &format!("/api/v1/{resource}/{id}"), None, None)
            .await;
        assert_eq!(status, StatusCode::OK);
        if resource == "jobs" {
            // Detail adds ledger statistics; the inventory projection stays
            // inexpensive and otherwise identical, including alias visibility.
            assert!(
                detail
                    .as_object_mut()
                    .unwrap()
                    .remove("statistics")
                    .is_some()
            );
        }
        assert_eq!(detail, body["items"][0]);
        assert_eq!(
            browser
                .request("GET", &format!("/api/v1/{resource}/{foreign}"), None, None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert!(!body.to_string().contains("credential"));
        assert!(!body.to_string().contains("secret/catalog"));
        // A job names its table with the same caller-visible aliases as the
        // table itself: the alias on the hidden connection never appears.
        let aliases = match resource {
            "tables" => Some(&body["items"][0]["aliases"]),
            "jobs" => Some(&body["items"][0]["table_aliases"]),
            _ => None,
        };
        if let Some(aliases) = aliases {
            assert_eq!(aliases.as_array().unwrap().len(), 1);
            assert_eq!(aliases[0]["catalog_id"], tenant.connection.to_string());
            assert_eq!(aliases[0]["name"], "events");
            assert!(!body.to_string().contains("hidden-alias"));
        }
    }
    assert_eq!(
        browser.request("GET", "/api/v1/audit", None, None).await.0,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cursors_are_bound_to_actor_resource_and_organization_and_recheck_grants(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    for _ in 0..3 {
        support::job(&mut conn, &tenant).await;
    }
    drop(conn);
    let (browser, grant) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let first = browser
        .request("GET", "/api/v1/jobs?limit=1", None, None)
        .await
        .1;
    let cursor = first["next_cursor"].as_str().unwrap();
    let second = browser
        .request(
            "GET",
            &format!("/api/v1/jobs?limit=1&cursor={cursor}"),
            None,
            None,
        )
        .await
        .1;
    assert_ne!(first["items"][0]["id"], second["items"][0]["id"]);
    assert_eq!(
        admin
            .request("GET", &format!("/api/v1/jobs?cursor={cursor}"), None, None)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        browser
            .request(
                "GET",
                &format!("/api/v1/tables?cursor={cursor}"),
                None,
                None
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("UPDATE principal_role_grants SET revoked_at = now() WHERE id = $1")
        .bind(grant.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let revoked = browser
        .request("GET", &format!("/api/v1/jobs?cursor={cursor}"), None, None)
        .await;
    assert_eq!(revoked.0, StatusCode::OK);
    assert!(revoked.1["items"].as_array().unwrap().is_empty());
    assert!(
        browser
            .request("GET", "/api/v1/catalogs", None, None)
            .await
            .1["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for invalid in [
        "limit=0",
        "limit=101",
        "limit=no",
        "cursor=bad",
        "unknown=1",
    ] {
        assert_eq!(
            admin
                .request("GET", &format!("/api/v1/jobs?{invalid}"), None, None)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn submission_is_atomic_idempotent_and_actor_bound_under_concurrency(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let body = json!({"table_id": tenant.table});
    let (left, right) = tokio::join!(
        browser.request("POST", "/api/v1/jobs", Some(body.clone()), Some("same")),
        browser.request("POST", "/api/v1/jobs", Some(body.clone()), Some("same"))
    );
    assert_eq!(left.0, StatusCode::ACCEPTED);
    assert_eq!(left, right);
    let id = left.1["job_id"].as_str().unwrap();
    assert_eq!(left.1["operation_id"], id);
    let job = browser
        .request("GET", &format!("/api/v1/jobs/{id}"), None, None)
        .await;
    assert_eq!(job.1["state"], "Planning");
    assert_eq!(job.1["created_by"], tenant.user.to_string());
    assert_eq!(job.1["created_by_name"], "alice");
    assert_eq!(job.1["revision"], 0);
    assert_eq!(job.1["publication_started"], false);
    let count: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM jobs), (SELECT count(*) FROM job_events), (SELECT count(*) FROM audit_events WHERE action = 'job.submitted')").fetch_one(&pool).await.unwrap();
    assert_eq!(count, (1, 1, 1));
    let (operator, _) = scoped(
        &pool,
        &tenant,
        Role::Operator,
        GrantScope::table(tenant.table),
    )
    .await;
    assert_eq!(
        operator
            .request("POST", "/api/v1/jobs", Some(body.clone()), Some("same"))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        operator
            .request("POST", "/api/v1/jobs", Some(body), Some("fresh"))
            .await
            .0,
        StatusCode::ACCEPTED
    );
    let audit = browser
        .request("GET", "/api/v1/audit?limit=1", None, None)
        .await;
    assert_eq!(audit.0, StatusCode::OK);
    assert!(audit.1["items"][0]["id"].is_string());
    assert!(audit.1["next_cursor"].is_string());
    let cursor = audit.1["next_cursor"].as_str().unwrap();
    let next = browser
        .request(
            "GET",
            &format!("/api/v1/audit?limit=1&cursor={cursor}"),
            None,
            None,
        )
        .await
        .1;
    assert_ne!(next["items"][0]["id"], audit.1["items"][0]["id"]);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn denied_and_invalid_submissions_create_no_job_and_csrf_is_required(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let foreign = support::tenant(&mut conn, "foreign").await;
    drop(conn);
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    let body = json!({"table_id": tenant.table});
    assert_eq!(
        viewer
            .request("POST", "/api/v1/jobs", Some(body.clone()), Some("x"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let mut admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    assert_eq!(
        admin
            .request(
                "POST",
                "/api/v1/jobs",
                Some(json!({"table_id": foreign.table})),
                Some("foreign")
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    for (body, key) in [
        (body.clone(), None),
        (body.clone(), Some("")),
        (
            json!({"table_id": tenant.table, "created_by": tenant.user}),
            Some("x"),
        ),
        (json!({"table_id": "bad"}), Some("x")),
    ] {
        assert_eq!(
            admin
                .request("POST", "/api/v1/jobs", Some(body), key)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    admin.csrf = "bad".into();
    assert_eq!(
        admin
            .request("POST", "/api/v1/jobs", Some(body), Some("x"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    admin.cookie.clear();
    assert_eq!(
        admin
            .request("POST", "/api/v1/jobs", Some(json!({})), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        admin
            .request("GET", "/api/v1/jobs?limit=bad", None, None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let denied: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE outcome = 'Denied'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(denied >= 3);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn configuration_resources_are_available_and_the_contract_is_public(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    for path in ["schedules", "policies"] {
        let (status, body) = admin
            .request("GET", &format!("/api/v1/{path}"), None, None)
            .await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(body["items"].as_array().unwrap().len(), 0);
        assert_eq!(body["next_cursor"], Value::Null);
    }
    let document = admin
        .request("GET", "/api/v1/openapi.json", None, None)
        .await;
    assert_eq!(document.0, StatusCode::OK);
    assert_eq!(document.1["openapi"], "3.1.0");
    let public = admin
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(public.status(), StatusCode::OK);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn audit_failure_rolls_back_submission_and_leaves_the_key_reusable(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    sqlx::query("CREATE FUNCTION reject_api_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test audit unavailable'; END $$").execute(&mut *conn).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_api_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_api_audit()").execute(&mut *conn).await.unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let body = json!({"table_id": tenant.table});
    let failed = browser
        .request(
            "POST",
            "/api/v1/jobs",
            Some(body.clone()),
            Some("retryable"),
        )
        .await;
    assert_eq!(failed.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(failed.1["disposition"], "PermanentFailure");
    let counts: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM jobs), (SELECT count(*) FROM job_events), (SELECT count(*) FROM idempotency_keys)").fetch_one(&pool).await.unwrap();
    assert_eq!(counts, (0, 0, 0));
    sqlx::query("DROP TRIGGER reject_api_audit ON audit_events")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        browser
            .request("POST", "/api/v1/jobs", Some(body), Some("retryable"))
            .await
            .0,
        StatusCode::ACCEPTED
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn conflicting_concurrent_submissions_create_only_one_job(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let table = tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(tenant.connection, "second"),
        Uuid::now_v7(),
    )
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (first, second) = tokio::join!(
        browser.request(
            "POST",
            "/api/v1/jobs",
            Some(json!({"table_id": tenant.table})),
            Some("one-key")
        ),
        browser.request(
            "POST",
            "/api/v1/jobs",
            Some(json!({"table_id": table.id})),
            Some("one-key")
        ),
    );
    let mut statuses = [first.0.as_u16(), second.0.as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, [202, 409]);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn audit_reads_are_tenant_scoped_and_replay_returns_original_after_transition(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other = support::tenant(&mut conn, "other").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let foreign = Browser::new(&pool, &other, other.user, "alice").await;
    let body = json!({"table_id": tenant.table});
    let original = browser
        .request("POST", "/api/v1/jobs", Some(body.clone()), Some("stable"))
        .await;
    assert_eq!(
        foreign
            .request(
                "POST",
                "/api/v1/jobs",
                Some(json!({"table_id": other.table})),
                Some("stable")
            )
            .await
            .0,
        StatusCode::ACCEPTED
    );
    let audit = browser.request("GET", "/api/v1/audit", None, None).await.1;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        audit["items"][0]["actor_principal_id"],
        tenant.user.to_string()
    );
    assert_eq!(audit["items"][0]["actor_display_name"], "alice");
    let id: JobId = original.1["job_id"].as_str().unwrap().parse().unwrap();
    let mut conn = pool.acquire().await.unwrap();
    support::force_job_state(&mut conn, id, JobState::NoOp).await;
    drop(conn);
    assert_eq!(
        original,
        browser
            .request("POST", "/api/v1/jobs", Some(body), Some("stable"))
            .await
    );
    let counts: (i64, i64) =
        sqlx::query_as("SELECT (SELECT count(*) FROM jobs), (SELECT count(*) FROM audit_events)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(counts, (2, 2));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn revoked_sessions_and_oversized_bodies_return_safe_errors(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let oversized = browser
        .request(
            "POST",
            "/api/v1/jobs",
            Some(json!({"table_id": "x".repeat(20000)})),
            Some("size"),
        )
        .await;
    assert_eq!(oversized.0, StatusCode::PAYLOAD_TOO_LARGE);
    sqlx::query("UPDATE auth_sessions SET revoked_at = now(), revoked_reason = 'test'")
        .execute(&pool)
        .await
        .unwrap();
    let rejected = browser.request("GET", "/api/v1/jobs", None, None).await;
    assert_eq!(rejected.0, StatusCode::UNAUTHORIZED);
    assert_eq!(rejected.1["reason"], "authentication_required");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn openapi_is_public_and_uses_the_configured_cookie(pool: PgPool) {
    let config = SessionConfig {
        secure: false,
        cookie_name: "custom_session".to_owned(),
        ..SessionConfig::default()
    };
    let router = router_with(&pool, config);
    let (status, headers, body) = raw(&router, "GET", "/api/v1/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(
        body["components"]["securitySchemes"]["sessionCookie"]["name"],
        "custom_session"
    );
    let (status, headers, body) = raw(&router, "POST", "/api/v1/openapi.json").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers["cache-control"], "no-store");
    assert!(
        headers["allow"]
            .to_str()
            .unwrap()
            .split(',')
            .any(|method| method.trim() == "GET")
    );
    assert_eq!(body["reason"], "method_not_allowed");
    assert_eq!(body["disposition"], "PermanentFailure");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn denied_preview_and_schedule_reads_are_audited(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    assert_eq!(
        viewer
            .request(
                "POST",
                "/api/v1/previews",
                Some(json!({"table_id": tenant.table})),
                None
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        viewer
            .request("GET", "/api/v1/schedules", None, None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let mut conn = pool.acquire().await.unwrap();
    let permissions: Vec<String> = sqlx::query_scalar(
        "SELECT detail->>'permission' FROM audit_events
         WHERE action = 'api.permission_denied' AND outcome = 'Denied'
         ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    assert_eq!(permissions, ["jobs:submit", "schedules:manage"]);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn audit_outage_does_not_change_denial_status(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    sqlx::query("CREATE FUNCTION reject_api_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test audit unavailable'; END $$").execute(&mut *conn).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_api_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_api_audit()").execute(&mut *conn).await.unwrap();
    drop(conn);
    let mut admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let body = json!({"table_id": tenant.table});
    let csrf = std::mem::take(&mut admin.csrf);
    assert_eq!(
        admin
            .request("POST", "/api/v1/jobs", Some(body.clone()), Some("csrf"))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    admin.csrf = csrf;
    admin.cookie.clear();
    assert_eq!(
        admin.request("GET", "/api/v1/jobs", None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    assert_eq!(
        viewer
            .request(
                "POST",
                "/api/v1/previews",
                Some(json!({"table_id": tenant.table})),
                None
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("DROP TRIGGER reject_api_audit ON audit_events")
        .execute(&pool)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn pagination_bounds_and_forged_audit_cursors_are_rejected(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let body = json!({"table_id": tenant.table});
    for key in ["first", "second"] {
        assert_eq!(
            admin
                .request("POST", "/api/v1/jobs", Some(body.clone()), Some(key))
                .await
                .0,
            StatusCode::ACCEPTED
        );
    }
    for limit in [1, 100] {
        assert_eq!(
            admin
                .request("GET", &format!("/api/v1/audit?limit={limit}"), None, None)
                .await
                .0,
            StatusCode::OK
        );
    }
    let page = admin
        .request("GET", "/api/v1/audit?limit=1", None, None)
        .await;
    let cursor = page.1["next_cursor"].as_str().unwrap();
    let bytes = URL_SAFE_NO_PAD.decode(cursor).unwrap();
    let mut value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["resource"], "audit");
    value["after"] = json!("-1");
    let forged = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap());
    assert_eq!(
        admin
            .request("GET", &format!("/api/v1/audit?cursor={forged}"), None, None)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let oversized = "a".repeat(1025);
    assert_eq!(
        admin
            .request(
                "GET",
                &format!("/api/v1/audit?cursor={oversized}"),
                None,
                None
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn many_concurrent_identical_submissions_share_one_operation(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let body = json!({"table_id": tenant.table});
    let responses = tokio::join!(
        browser.request("POST", "/api/v1/jobs", Some(body.clone()), Some("shared")),
        browser.request("POST", "/api/v1/jobs", Some(body.clone()), Some("shared")),
        browser.request("POST", "/api/v1/jobs", Some(body.clone()), Some("shared")),
        browser.request("POST", "/api/v1/jobs", Some(body.clone()), Some("shared")),
        browser.request("POST", "/api/v1/jobs", Some(body.clone()), Some("shared")),
        browser.request("POST", "/api/v1/jobs", Some(body.clone()), Some("shared")),
        browser.request("POST", "/api/v1/jobs", Some(body.clone()), Some("shared")),
        browser.request("POST", "/api/v1/jobs", Some(body), Some("shared")),
    );
    let responses = [
        responses.0,
        responses.1,
        responses.2,
        responses.3,
        responses.4,
        responses.5,
        responses.6,
        responses.7,
    ];
    let first = responses[0].1["job_id"].as_str().unwrap();
    for (status, body) in &responses {
        assert_eq!(*status, StatusCode::ACCEPTED);
        assert_eq!(body["job_id"].as_str(), Some(first));
    }
    let counts: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM jobs), (SELECT count(*) FROM job_events), (SELECT count(*) FROM idempotency_keys)").fetch_one(&pool).await.unwrap();
    assert_eq!(counts, (1, 1, 1));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn ledger_outage_is_retryable(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    pool.close().await;
    let (status, body) = browser.request("GET", "/api/v1/jobs", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["disposition"], "RetrySameRequest");
}

#[test]
fn openapi_documents_every_route() {
    // Keep in sync with `api::router`.
    const ROUTES: &[(&str, &str)] = &[
        ("/api/v1/catalogs", "get"),
        ("/api/v1/catalogs", "post"),
        ("/api/v1/catalogs/check", "post"),
        ("/api/v1/catalogs/{id}", "get"),
        ("/api/v1/catalogs/{id}", "patch"),
        ("/api/v1/catalogs/{id}/check", "post"),
        ("/api/v1/tables", "get"),
        ("/api/v1/tables/{id}", "get"),
        ("/api/v1/tables/{id}/snapshots", "get"),
        ("/api/v1/tables/{id}/definition", "get"),
        ("/api/v1/tables/{id}/maintenance", "get"),
        ("/api/v1/tables/{id}/recommendations", "get"),
        ("/api/v1/statistics/maintenance", "get"),
        ("/api/v1/overview", "get"),
        ("/api/v1/tables/{id}/statistics", "get"),
        ("/api/v1/catalogs/{id}/statistics", "get"),
        ("/api/v1/statistics", "get"),
        ("/api/v1/jobs", "get"),
        ("/api/v1/jobs", "post"),
        ("/api/v1/jobs/{id}", "get"),
        ("/api/v1/jobs/{id}/cancel", "post"),
        ("/api/v1/jobs/{id}/retry", "post"),
        ("/api/v1/jobs/{id}/events", "get"),
        ("/api/v1/audit", "get"),
        ("/api/v1/workers", "get"),
        ("/api/v1/previews", "post"),
        ("/api/v1/schedules", "get"),
        ("/api/v1/schedules", "post"),
        ("/api/v1/schedules/{id}", "get"),
        ("/api/v1/schedules/{id}", "patch"),
        ("/api/v1/schedules/{id}/occurrences", "get"),
        ("/api/v1/policies", "get"),
        ("/api/v1/policies", "post"),
        ("/api/v1/policies/{id}", "get"),
        ("/api/v1/policies/{id}", "patch"),
        ("/api/v1/catalogs/{id}/namespaces", "get"),
        ("/api/v1/catalogs/{id}/tables", "get"),
        ("/api/v1/catalogs/{id}/registrations", "post"),
        ("/api/v1/catalogs/{id}/registrations", "delete"),
        ("/api/v1/principals", "get"),
        ("/api/v1/principals", "post"),
        ("/api/v1/principals/{id}", "get"),
        ("/api/v1/principals/{id}", "patch"),
        ("/api/v1/grants", "get"),
        ("/api/v1/grants", "post"),
        ("/api/v1/grants/{id}", "delete"),
        ("/api/v1/service-identities", "get"),
        ("/api/v1/service-identities", "post"),
        ("/api/v1/service-identities/{id}", "get"),
        ("/api/v1/service-identities/{id}/tokens", "get"),
        ("/api/v1/service-identities/{id}/tokens", "post"),
        ("/api/v1/service-identities/{id}/tokens/{token}", "get"),
        ("/api/v1/service-identities/{id}/tokens/{token}", "delete"),
        (
            "/api/v1/service-identities/{id}/tokens/{token}/rotate",
            "post",
        ),
        ("/api/v1/openapi.json", "get"),
    ];
    let document: Value = serde_json::from_str(include_str!("../src/api/openapi.json")).unwrap();
    let documented: BTreeSet<(&str, &str)> = document["paths"]
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(path, operations)| {
            ["get", "post", "patch", "delete"]
                .into_iter()
                .filter_map(move |method| {
                    operations
                        .get(method)
                        .is_some()
                        .then_some((path.as_str(), method))
                })
        })
        .collect();
    let expected: BTreeSet<(&str, &str)> = ROUTES.iter().copied().collect();
    assert_eq!(documented, expected);
    assert!(
        document["paths"]["/api/v1/openapi.json"]["get"]["responses"]["405"].is_object(),
        "the public contract route documents its method-not-allowed response"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn well_formed_unknown_tokens_are_rejected_without_audit_rows(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let cookie_name = SessionConfig::default().cookie_name;
    for _ in 0..3 {
        let token = session::SessionToken::generate().unwrap();
        let response = browser
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/jobs")
                    .header("cookie", format!("{cookie_name}={}", token.expose()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn openapi_declares_a_required_parameter_for_every_path_placeholder() {
    let document: Value = serde_json::from_str(include_str!("../src/api/openapi.json")).unwrap();
    for (path, operations) in document["paths"].as_object().unwrap() {
        let mut names = Vec::new();
        let mut rest = path.as_str();
        while let Some(start) = rest.find('{') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('}') else { break };
            names.push(&after[..end]);
            rest = &after[end + 1..];
        }
        for method in ["get", "post"] {
            let Some(operation) = operations.get(method) else {
                continue;
            };
            let mut parameters: Vec<&Value> = Vec::new();
            for source in [operations.get("parameters"), operation.get("parameters")] {
                if let Some(list) = source.and_then(Value::as_array) {
                    parameters.extend(list);
                }
            }
            let declared = |name: &str| {
                parameters
                    .iter()
                    .find(|parameter| parameter["name"] == name && parameter["in"] == "path")
            };
            for name in &names {
                let parameter = declared(name).unwrap_or_else(|| {
                    panic!("{path} {method} does not declare path parameter {name}")
                });
                assert_eq!(parameter["required"], true, "{path} {method} {name}");
                assert_eq!(
                    parameter["schema"]["type"], "string",
                    "{path} {method} {name}"
                );
                assert_eq!(
                    parameter["schema"]["format"], "uuid",
                    "{path} {method} {name}"
                );
            }
            // A declared path parameter that is not in the path is a contract bug too.
            for parameter in &parameters {
                if parameter["in"] == "path" {
                    let name = parameter["name"].as_str().unwrap();
                    assert!(
                        names.contains(&name),
                        "{path} {method} declares unused path parameter {name}"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BS-018.T2: previews and job commands.
// ---------------------------------------------------------------------------

use std::sync::Arc;

use bergsmith_common::ids::CatalogConnectionId;
use bergsmith_common::job::JobState;
use bergsmith_fixtures::{Commit, FileSpec, TableSpec, Value as FixtureValue, row};
use bergsmith_persistence::tables;
use bergsmith_persistence::tenancy::{self, NewCatalogConnection};
use iceberg::spec::{
    FormatVersion, NestedField, PrimitiveType, Schema, SortOrder, TableMetadataBuilder, Transform,
    Type, UnboundPartitionSpec,
};
use mockito::{Mock, ServerGuard};
use sqlx::PgConnection;
use uuid::Uuid;

fn schema_with_id() -> Schema {
    Schema::builder()
        .with_fields([Arc::new(NestedField::required(
            1,
            "id",
            Type::Primitive(PrimitiveType::Long),
        ))])
        .build()
        .unwrap()
}

/// Five small files, so the default planner configuration admits one task.
async fn fixture_table() -> bergsmith_fixtures::Fixture {
    let mut commit = Commit::new();
    for index in 0..5_i64 {
        commit = commit.file(FileSpec::data(format!("d{index}"), [row![1 => index]]));
    }
    TableSpec::new(schema_with_id())
        .commit(commit)
        .generate()
        .await
        .unwrap()
}

/// A single small file: eligible, but never worth a job on its own.
async fn no_op_table() -> bergsmith_fixtures::Fixture {
    TableSpec::new(schema_with_id())
        .commit(Commit::new().file(FileSpec::data("d0", [row![1 => 0_i64]])))
        .generate()
        .await
        .unwrap()
}

/// An equality delete keyed on a float: the MVP must refuse the table.
async fn rejected_table() -> bergsmith_fixtures::Fixture {
    let schema = Schema::builder()
        .with_fields([
            Arc::new(NestedField::required(
                1,
                "id",
                Type::Primitive(PrimitiveType::Long),
            )),
            Arc::new(NestedField::optional(
                2,
                "score",
                Type::Primitive(PrimitiveType::Float),
            )),
        ])
        .build()
        .unwrap();
    TableSpec::new(schema)
        .commit(
            Commit::new()
                .file(FileSpec::data(
                    "d0",
                    [row![1 => 1_i64, 2 => FixtureValue::float(1.5)]],
                ))
                .file(FileSpec::equality_deletes(
                    "x0",
                    [2],
                    [row![2 => FixtureValue::float(1.5)]],
                )),
        )
        .generate()
        .await
        .unwrap()
}

/// Two partitions with five small files each: two candidate bins.
async fn partitioned_table() -> bergsmith_fixtures::Fixture {
    let schema = Schema::builder()
        .with_fields([
            Arc::new(NestedField::required(
                1,
                "id",
                Type::Primitive(PrimitiveType::Long),
            )),
            Arc::new(NestedField::optional(
                2,
                "tag",
                Type::Primitive(PrimitiveType::String),
            )),
        ])
        .build()
        .unwrap();
    let spec = UnboundPartitionSpec::builder()
        .add_partition_field(2, "tag", Transform::Identity)
        .unwrap()
        .build();
    let mut commit = Commit::new();
    for tag in ["a", "b"] {
        for index in 0..5_i64 {
            commit = commit.file(
                FileSpec::data(format!("{tag}{index}"), [row![1 => index]])
                    .partition([FixtureValue::from(tag)]),
            );
        }
    }
    TableSpec::new(schema)
        .partitioned_by(spec)
        .commit(commit)
        .generate()
        .await
        .unwrap()
}

async fn connection_to(
    conn: &mut PgConnection,
    organization: bergsmith_common::ids::OrganizationId,
    name: &str,
    uri: &str,
) -> CatalogConnectionId {
    let id = CatalogConnectionId::generate();
    tenancy::create_catalog_connection(
        conn,
        &NewCatalogConnection {
            id,
            organization_id: organization,
            name: name.to_owned(),
            uri: uri.to_owned(),
            warehouse: None,
            credential_ref: None,
        },
    )
    .await
    .unwrap();
    id
}

/// A tenant whose `main` connection reaches `uri` and whose table is
/// `table_uuid`, owned by an administrator.
async fn preview_tenant(
    conn: &mut PgConnection,
    slug: &str,
    uri: &str,
    table_uuid: Uuid,
) -> support::Tenant {
    let organization = support::organization(conn, slug).await;
    let user = support::user(conn, organization, "alice").await;
    let connection = connection_to(conn, organization, "main", uri).await;
    let table = tables::resolve_table(
        conn,
        organization,
        &support::alias(connection, "events"),
        table_uuid,
    )
    .await
    .unwrap()
    .id;
    grants::create_grant(
        conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: organization,
            principal_id: user,
            role: Role::Administrator,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    support::Tenant {
        organization,
        user,
        connection,
        table,
    }
}

async fn catalog_config_mock(server: &mut ServerGuard) -> Mock {
    server
        .mock("GET", "/v1/config")
        .with_body("{}")
        .create_async()
        .await
}

async fn load_mock(
    server: &mut ServerGuard,
    fixture: &bergsmith_fixtures::Fixture,
    table: &str,
) -> Mock {
    let path = format!("/v1/namespaces/db/tables/{table}");
    server
        .mock("GET", path.as_str())
        .with_body(
            json!({"metadata-location": fixture.metadata_location(), "metadata": fixture.metadata()})
                .to_string(),
        )
        .create_async()
        .await
}

/// Metadata for a replacement table at `location`.
fn foreign_metadata(uuid: Uuid, location: &str) -> iceberg::spec::TableMetadata {
    TableMetadataBuilder::new(
        schema_with_id(),
        UnboundPartitionSpec::builder().build(),
        SortOrder::unsorted_order(),
        location.to_owned(),
        FormatVersion::V2,
        std::collections::HashMap::new(),
    )
    .unwrap()
    .assign_uuid(uuid)
    .build()
    .unwrap()
    .metadata
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn preview_assesses_a_table_without_creating_work(pool: PgPool) {
    let fixture = fixture_table().await;
    let mut server = mockito::Server::new_async().await;
    let config = catalog_config_mock(&mut server).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme", &server.url(), fixture.metadata().uuid()).await;
    let load = load_mock(&mut server, &fixture, "events").await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["table_id"], tenant.table.to_string());
    assert_eq!(body["outcome"], "planned");
    assert_eq!(body["no_op_reason"], Value::Null);
    assert_eq!(body["candidates"]["files"], 5);
    assert_eq!(body["bins"].as_array().unwrap().len(), 1);
    assert_eq!(body["bins"][0]["disposition"], "admitted");
    assert_eq!(body["bins_total"], 1);
    assert_eq!(body["bins_truncated"], false);
    assert_eq!(body["estimate"]["tasks"], 1);
    assert_eq!(
        body["baseline"]["snapshot_id"].as_str(),
        fixture
            .metadata()
            .current_snapshot_id()
            .map(|id| id.to_string())
            .as_deref()
    );
    // The baseline exposes no metadata or table location.
    assert!(body["baseline"].get("metadata_location").is_none());
    assert!(body.get("metadata_location").is_none());
    config.assert_async().await;
    load.assert_async().await;
    let counts: (i64, i64) =
        sqlx::query_as("SELECT (SELECT count(*) FROM jobs), (SELECT count(*) FROM audit_events)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(counts, (0, 0), "a preview creates no job and no audit row");
}

/// A sample whose small files are critical and whose snapshot backlog is a
/// warning. Both recommendation tests start from it.
fn health_sample(
    connection: CatalogConnectionId,
    at: chrono::DateTime<chrono::Utc>,
) -> bergsmith_persistence::statistics::NewTableSample {
    bergsmith_persistence::statistics::NewTableSample {
        catalog_connection_id: connection,
        sampled_at: at,
        metadata_location: "s3://bucket/table/metadata/v1.metadata.json".to_owned(),
        snapshot_id: Some(1),
        last_operation: Some("append".to_owned()),
        record_count: 5,
        data_files: 5,
        // Four MiB per file: below the critical small-file bound.
        data_bytes: 5 * 4 * 1024 * 1024,
        delete_files: 0,
        delete_bytes: 0,
        position_delete_files: 0,
        equality_delete_files: 0,
        position_delete_records: 0,
        equality_delete_records: 0,
        manifest_count: 1,
        data_manifest_count: 1,
        manifest_bytes: 1_024,
        partition_count: 0,
        max_partition_files: 0,
        small_files: 5,
        target_file_size_bytes: 512 * 1024 * 1024,
        // Above the snapshot warning bound, which is advisory.
        snapshot_count: 1_500,
        oldest_snapshot_at: Some(at),
        newest_snapshot_at: Some(at),
        size_histogram: json!([]),
        top_partitions: json!([]),
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn recommendations_pair_findings_with_the_planner_estimate(pool: PgPool) {
    use bergsmith_persistence::statistics as samples;

    let fixture = fixture_table().await;
    let mut server = mockito::Server::new_async().await;
    let config = catalog_config_mock(&mut server).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme", &server.url(), fixture.metadata().uuid()).await;
    let load = load_mock(&mut server, &fixture, "events").await;
    let at = chrono::DateTime::from_timestamp_micros(chrono::Utc::now().timestamp_micros())
        .expect("the current instant");
    samples::record(
        &mut conn,
        tenant.organization,
        tenant.table,
        &health_sample(tenant.connection, at),
    )
    .await
    .unwrap();
    samples::record_check(&mut conn, tenant.organization, tenant.table, at)
        .await
        .unwrap();
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/tables/{}/recommendations", tenant.table);
    let (status, body) = admin.request("GET", &path, None, None).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "critical");
    assert_eq!(
        body["sampled_at"],
        at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    );
    assert_eq!(body["findings"].as_array().unwrap().len(), 2);
    assert_eq!(body["findings"][0]["kind"], "small_files");
    assert_eq!(body["findings"][0]["severity"], "critical");
    assert_eq!(body["findings"][0]["measured"], "4194304");
    assert_eq!(body["findings"][0]["unit"], "bytes");
    assert_eq!(body["findings"][1]["kind"], "snapshot_backlog");

    // The actionable finding reuses the preview path and carries its estimate.
    assert_eq!(body["recommendations"][0]["kind"], "small_files");
    assert_eq!(body["recommendations"][0]["action"], "compaction");
    assert_eq!(body["recommendations"][0]["advisory"], false);
    assert_eq!(body["recommendations"][0]["reason"], Value::Null);
    assert_eq!(
        body["recommendations"][0]["preview"]["table_id"],
        tenant.table.to_string()
    );
    assert_eq!(body["recommendations"][0]["outcome"], "planned");
    assert_eq!(body["recommendations"][0]["estimate"]["tasks"], "1");
    assert_eq!(body["recommendations"][0]["estimate"]["files"], "5");

    // The M7 finding stays advisory and carries no estimate.
    assert_eq!(body["recommendations"][1]["kind"], "snapshot_backlog");
    assert_eq!(body["recommendations"][1]["action"], "snapshot_expiration");
    assert_eq!(body["recommendations"][1]["advisory"], true);
    assert!(body["recommendations"][1]["reason"].is_string());
    assert_eq!(body["recommendations"][1]["preview"], Value::Null);
    assert_eq!(body["recommendations"][1]["estimate"], Value::Null);
    config.assert_async().await;
    load.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn recommendations_without_a_sample_do_not_touch_the_catalog(pool: PgPool) {
    let fixture = fixture_table().await;
    // No catalog mock exists: a catalog request would fail the response.
    let server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme", &server.url(), fixture.metadata().uuid()).await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/tables/{}/recommendations", tenant.table);
    let (status, body) = admin.request("GET", &path, None, None).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], Value::Null);
    assert_eq!(body["sampled_at"], Value::Null);
    assert_eq!(body["confirmed_at"], Value::Null);
    assert_eq!(body["findings"], json!([]));
    assert_eq!(body["recommendations"], json!([]));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn recommendations_hide_tables_outside_the_callers_grants(pool: PgPool) {
    let fixture = fixture_table().await;
    let server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme", &server.url(), fixture.metadata().uuid()).await;
    // No grants: the table is outside the caller's inventory.
    let outsider = support::user(&mut conn, tenant.organization, "bob").await;
    // Views inventory but may not submit: a visible table, denied with 403.
    let viewer = support::user(&mut conn, tenant.organization, "viewer").await;
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: viewer,
            role: Role::Viewer,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    drop(conn);
    let path = format!("/api/v1/tables/{}/recommendations", tenant.table);

    let outside = Browser::new(&pool, &tenant, outsider, "bob").await;
    let (status, _) = outside.request("GET", &path, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let unknown = format!("/api/v1/tables/{}/recommendations", Uuid::now_v7());
    let (status, _) = outside.request("GET", &unknown, None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let reader = Browser::new(&pool, &tenant, viewer, "viewer").await;
    let (status, _) = reader.request("GET", &path, None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn recommendations_treat_an_incomplete_sample_as_unassessed(pool: PgPool) {
    let fixture = fixture_table().await;
    // No catalog mock exists: a catalog request would fail the response.
    let server = mockito::Server::new_async().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme", &server.url(), fixture.metadata().uuid()).await;
    let at = chrono::DateTime::from_timestamp_micros(chrono::Utc::now().timestamp_micros())
        .expect("the current instant");
    bergsmith_persistence::statistics::record(
        &mut conn,
        tenant.organization,
        tenant.table,
        &health_sample(tenant.connection, at),
    )
    .await
    .unwrap();
    bergsmith_persistence::statistics::record_check(
        &mut conn,
        tenant.organization,
        tenant.table,
        at,
    )
    .await
    .unwrap();
    // A sample written before the health counters existed: version 0.
    sqlx::query("UPDATE table_statistics SET sample_version = 0")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/tables/{}/recommendations", tenant.table);
    let (status, body) = admin.request("GET", &path, None, None).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], Value::Null);
    assert_eq!(
        body["sampled_at"],
        at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
    );
    assert_eq!(body["findings"], json!([]));
    assert_eq!(body["recommendations"], json!([]));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn preview_never_contacts_a_connection_outside_the_callers_grant(pool: PgPool) {
    let fixture = fixture_table().await;
    let mut outside_server = mockito::Server::new_async().await;
    let mut granted_server = mockito::Server::new_async().await;
    let outside_config = outside_server
        .mock("GET", "/v1/config")
        .with_status(503)
        .expect(0)
        .create_async()
        .await;
    let config = catalog_config_mock(&mut granted_server).await;
    let mut conn = pool.acquire().await.unwrap();
    let organization = support::organization(&mut conn, "acme").await;
    let principal = support::user(&mut conn, organization, "operator").await;
    let outside = connection_to(&mut conn, organization, "outside", &outside_server.url()).await;
    let granted = connection_to(&mut conn, organization, "granted", &granted_server.url()).await;
    let table = tables::resolve_table(
        &mut conn,
        organization,
        &support::alias(outside, "events"),
        fixture.metadata().uuid(),
    )
    .await
    .unwrap()
    .id;
    tables::resolve_table(
        &mut conn,
        organization,
        &support::alias(granted, "events_v"),
        fixture.metadata().uuid(),
    )
    .await
    .unwrap();
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: organization,
            principal_id: principal,
            role: Role::Operator,
            scope: GrantScope::connection(granted),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    let load = load_mock(&mut granted_server, &fixture, "events_v").await;
    drop(conn);
    let tenant = support::Tenant {
        organization,
        user: principal,
        connection: granted,
        table,
    };
    let browser = Browser::new(&pool, &tenant, principal, "operator").await;
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcome"], "planned");
    config.assert_async().await;
    load.assert_async().await;
    outside_config.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn preview_never_contacts_the_catalog_for_a_denied_caller(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_body("{}")
        .expect(0)
        .create_async()
        .await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme", &server.url(), Uuid::now_v7()).await;
    drop(conn);
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    let (status, body) = viewer
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["reason"], "permission_denied");
    config.assert_async().await;
    let denied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE action = 'api.permission_denied' AND detail->>'permission' = 'jobs:submit'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(denied, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn preview_reports_no_op_rejected_and_truncated_assessments(pool: PgPool) {
    let mut server = mockito::Server::new_async().await;
    catalog_config_mock(&mut server).await;

    // Nothing beneficial to do.
    let no_op = no_op_table().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(
        &mut conn,
        "acme-noop",
        &server.url(),
        no_op.metadata().uuid(),
    )
    .await;
    load_mock(&mut server, &no_op, "events").await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcome"], "no_op");
    assert_eq!(body["no_op_reason"], "no_beneficial_work");
    assert_eq!(body["estimate"]["tasks"], 0);
    assert_eq!(body["bins"].as_array().unwrap().len(), 1);
    assert_eq!(body["bins"][0]["disposition"], "no_benefit");

    // A refused table.
    let rejected = rejected_table().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(
        &mut conn,
        "acme-rejected",
        &server.url(),
        rejected.metadata().uuid(),
    )
    .await;
    load_mock(&mut server, &rejected, "events").await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcome"], "rejected");
    assert_eq!(body["no_op_reason"], Value::Null);
    assert_eq!(body["rejections"][0]["reason"], "equality_key");
    assert_eq!(body["rejections"][0]["problem"], "floating_point");
    assert!(body["rejections"][0]["detail"].is_string());

    // A table with more bins than one response returns.
    let partitioned = partitioned_table().await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(
        &mut conn,
        "acme-partitioned",
        &server.url(),
        partitioned.metadata().uuid(),
    )
    .await;
    load_mock(&mut server, &partitioned, "events").await;
    drop(conn);
    let limited = CatalogPreviewer::with_limits(
        PreviewProfiles::new(StorageProfiles::new([StorageProfile::LocalFs])),
        PlannerConfig::default(),
        PreviewLimits {
            max_bins: 1,
            ..PreviewLimits::default()
        },
    )
    .unwrap();
    let admin = Browser::with_previews(&pool, &tenant, tenant.user, "alice", limited).await;
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcome"], "planned");
    assert_eq!(body["bins_total"], 2);
    assert_eq!(body["bins"].as_array().unwrap().len(), 1);
    assert_eq!(body["bins_truncated"], true);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn preview_reports_identity_change_before_storage_configuration_errors(pool: PgPool) {
    // The replacement table's backend has no configured profile. The identity
    // change must win over the storage error.
    let mut server = mockito::Server::new_async().await;
    catalog_config_mock(&mut server).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme-foreign-gcs", &server.url(), Uuid::now_v7()).await;
    let foreign = foreign_metadata(Uuid::now_v7(), "gs://foreign-bucket/t");
    let mocked = server
        .mock("GET", "/v1/namespaces/db/tables/events")
        .with_body(json!({"metadata": foreign}).to_string())
        .create_async()
        .await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["reason"], "table_identity_changed");
    assert_eq!(body["disposition"], "PermanentFailure");
    mocked.assert_async().await;

    // The replacement table is S3 and its catalog vends no credentials. A
    // credential error must not mask the identity change, and the delegated
    // load must never happen.
    let mut server = mockito::Server::new_async().await;
    catalog_config_mock(&mut server).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme-foreign-s3", &server.url(), Uuid::now_v7()).await;
    let foreign = foreign_metadata(Uuid::now_v7(), "s3://warehouse/foreign");
    let mocked = server
        .mock("GET", "/v1/namespaces/db/tables/events")
        .with_body(json!({"metadata": foreign}).to_string())
        .expect(1)
        .create_async()
        .await;
    drop(conn);
    let s3 = CatalogPreviewer::new(
        PreviewProfiles::new(StorageProfiles::new([StorageProfile::S3(S3Profile {
            endpoint: None,
            region: "us-east-1".to_owned(),
            path_style_access: false,
        })])),
        PlannerConfig::default(),
    )
    .unwrap();
    let admin = Browser::with_previews(&pool, &tenant, tenant.user, "alice", s3).await;
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["reason"], "table_identity_changed");
    mocked.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn preview_reports_catalog_identity_and_configuration_failures(pool: PgPool) {
    let fixture = fixture_table().await;
    let mut server = mockito::Server::new_async().await;
    catalog_config_mock(&mut server).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme", &server.url(), fixture.metadata().uuid()).await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    // No such canonical table.
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": Uuid::now_v7()})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["reason"], "resource_not_found");

    // The catalog has no such table.
    let missing = server
        .mock("GET", "/v1/namespaces/db/tables/events")
        .with_status(404)
        .create_async()
        .await;
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["reason"], "catalog_table_missing");
    missing.remove_async().await;

    // The catalog is unavailable.
    let broken = server
        .mock("GET", "/v1/namespaces/db/tables/events")
        .with_status(503)
        .create_async()
        .await;
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["reason"], "catalog_unavailable");
    assert_eq!(body["disposition"], "RetrySameRequest");
    broken.remove_async().await;

    // The catalog now reports a different table identity. The fixture's
    // `table-uuid` is fixed, so build metadata for another table.
    let foreign = TableMetadataBuilder::new(
        schema_with_id(),
        UnboundPartitionSpec::builder().build(),
        SortOrder::unsorted_order(),
        "file:///tmp/foreign".to_owned(),
        FormatVersion::V2,
        std::collections::HashMap::new(),
    )
    .unwrap()
    .assign_uuid(Uuid::now_v7())
    .build()
    .unwrap()
    .metadata;
    let replaced = server
        .mock("GET", "/v1/namespaces/db/tables/events")
        .with_body(json!({"metadata": foreign}).to_string())
        .create_async()
        .await;
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["reason"], "table_identity_changed");
    assert_eq!(body["disposition"], "PermanentFailure");
    replaced.remove_async().await;

    // A stored credential reference cannot be resolved yet; no catalog call.
    let refused_config = server
        .mock("GET", "/v1/config")
        .with_body("{}")
        .expect(0)
        .create_async()
        .await;
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("UPDATE catalog_connections SET credential_ref = 'secret/catalog' WHERE id = $1")
        .bind(tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["reason"], "catalog_credential_unavailable");
    assert_eq!(body["disposition"], "PermanentFailure");
    refused_config.assert_async().await;

    // A missing storage profile for the table's location.
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("UPDATE catalog_connections SET credential_ref = NULL WHERE id = $1")
        .bind(tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let load = load_mock(&mut server, &fixture, "events").await;
    let unconfigured = CatalogPreviewer::new(
        PreviewProfiles::new(StorageProfiles::new([])),
        PlannerConfig::default(),
    )
    .unwrap();
    let admin = Browser::with_previews(&pool, &tenant, tenant.user, "alice", unconfigured).await;
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["reason"], "storage_profile_unavailable");
    load.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn preview_reports_permanent_catalog_configuration_failures(pool: PgPool) {
    // The catalog rejects the coordinator's credentials: an operator must fix
    // the connection, so the client is not told to repeat the request.
    let fixture = fixture_table().await;
    let mut server = mockito::Server::new_async().await;
    catalog_config_mock(&mut server).await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(
        &mut conn,
        "acme-unauthorized",
        &server.url(),
        fixture.metadata().uuid(),
    )
    .await;
    let unauthorized = server
        .mock("GET", "/v1/namespaces/db/tables/events")
        .with_status(401)
        .create_async()
        .await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["reason"], "catalog_access_denied");
    assert_eq!(body["disposition"], "PermanentFailure");
    unauthorized.assert_async().await;

    // An S3 table whose catalog does not vend credentials needs the catalog's
    // credential vending configured; retrying unchanged cannot help.
    let table_uuid = Uuid::now_v7();
    let metadata = TableMetadataBuilder::new(
        schema_with_id(),
        UnboundPartitionSpec::builder().build(),
        SortOrder::unsorted_order(),
        "s3://warehouse/t".to_owned(),
        FormatVersion::V2,
        std::collections::HashMap::new(),
    )
    .unwrap()
    .assign_uuid(table_uuid)
    .build()
    .unwrap()
    .metadata;
    let mut unvended = mockito::Server::new_async().await;
    catalog_config_mock(&mut unvended).await;
    unvended
        .mock("GET", "/v1/namespaces/db/tables/events")
        .with_body(
            json!({
                "metadata-location": "s3://warehouse/t/metadata/v1.json",
                "metadata": metadata,
            })
            .to_string(),
        )
        .create_async()
        .await;
    let mut conn = pool.acquire().await.unwrap();
    let tenant = preview_tenant(&mut conn, "acme-unvended", &unvended.url(), table_uuid).await;
    drop(conn);
    let s3 = CatalogPreviewer::new(
        PreviewProfiles::new(StorageProfiles::new([StorageProfile::S3(S3Profile {
            endpoint: None,
            region: "us-east-1".to_owned(),
            path_style_access: false,
        })])),
        PlannerConfig::default(),
    )
    .unwrap();
    let admin = Browser::with_previews(&pool, &tenant, tenant.user, "alice", s3).await;
    let (status, body) = admin
        .request(
            "POST",
            "/api/v1/previews",
            Some(json!({"table_id": tenant.table})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["reason"], "catalog_credentials_unavailable");
    assert_eq!(body["disposition"], "PermanentFailure");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_reports_its_outcome_and_keeps_the_first_request(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let foreign = support::tenant(&mut conn, "other").await;
    let job = support::job(&mut conn, &tenant).await;
    let ended = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, ended, JobState::NoOp).await;
    let cancelled = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, cancelled, JobState::Cancelled).await;
    let publishing = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, publishing, JobState::Committing).await;
    sqlx::query("UPDATE jobs SET publication_started_at = now() WHERE id = $1")
        .bind(publishing.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let unresolved = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, unresolved, JobState::CommitOutcomeUnknown).await;
    sqlx::query("UPDATE jobs SET publication_started_at = now() WHERE id = $1")
        .bind(unresolved.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let foreign_job = support::job(&mut conn, &foreign).await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, body) = admin
        .request(
            "POST",
            &format!("/api/v1/jobs/{job}/cancel"),
            Some(json!({"reason": "operator maintenance"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["cancellation"], "cancelling");
    assert_eq!(body["state"], "Cancelling");
    assert_eq!(body["revision"], 1);
    assert_eq!(body["cancellation_requested"], true);
    assert_eq!(body["publication_started"], false);

    // Repeating the request changes nothing and returns the same outcome.
    let repeated = admin
        .request(
            "POST",
            &format!("/api/v1/jobs/{job}/cancel"),
            Some(json!({"reason": "a second reason"})),
            None,
        )
        .await;
    assert_eq!(repeated, (status, body.clone()));

    let (status, body) = admin
        .request("POST", &format!("/api/v1/jobs/{ended}/cancel"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["cancellation"], "already_ended");
    assert_eq!(body["state"], "NoOp");

    let (status, body) = admin
        .request(
            "POST",
            &format!("/api/v1/jobs/{cancelled}/cancel"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["cancellation"], "cancelled");
    assert_eq!(body["state"], "Cancelled");

    let (status, body) = admin
        .request(
            "POST",
            &format!("/api/v1/jobs/{publishing}/cancel"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["cancellation"], "too_late_publication_started");
    assert_eq!(body["state"], "Committing");
    assert_eq!(body["publication_started"], true);

    let (status, body) = admin
        .request(
            "POST",
            &format!("/api/v1/jobs/{unresolved}/cancel"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["cancellation"], "publication_unresolved");
    assert_eq!(body["state"], "CommitOutcomeUnknown");

    assert_eq!(
        admin
            .request(
                "POST",
                &format!("/api/v1/jobs/{foreign_job}/cancel"),
                None,
                None
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        admin
            .request(
                "POST",
                &format!("/api/v1/jobs/{job}/cancel"),
                Some(json!({"reason": "x".repeat(501)})),
                None
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        admin
            .request(
                "POST",
                &format!("/api/v1/jobs/{job}/cancel"),
                Some(json!({"unknown": true})),
                None
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        admin
            .request(
                "POST",
                &format!("/api/v1/jobs/{job}/cancel"),
                Some(json!({"reason": "x".repeat(20_000)})),
                None
            )
            .await
            .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );

    let row: (String, Option<String>) =
        sqlx::query_as("SELECT state, cancellation_reason FROM jobs WHERE id = $1")
            .bind(job.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0, "Cancelling");
    assert_eq!(row.1.as_deref(), Some("operator maintenance"));

    // The audit records the persisted reason, not the repeated one.
    let audited: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT detail->>'reason' FROM audit_events
         WHERE action = 'job.cancellation_requested' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(audited.len(), 6);
    assert_eq!(audited[0].as_deref(), Some("operator maintenance"));
    assert_eq!(audited[1].as_deref(), Some("operator maintenance"));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_audit_failure_leaves_the_job_unchanged(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    sqlx::query("CREATE FUNCTION reject_api_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test audit unavailable'; END $$").execute(&mut *conn).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_api_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_api_audit()").execute(&mut *conn).await.unwrap();
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, body) = admin
        .request(
            "POST",
            &format!("/api/v1/jobs/{job}/cancel"),
            Some(json!({"reason": "audited"})),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["disposition"], "PermanentFailure");
    let row: (String, bool) = sqlx::query_as(
        "SELECT state, cancellation_requested_at IS NOT NULL FROM jobs WHERE id = $1",
    )
    .bind(job.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row, ("Planning".to_owned(), false));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_requires_the_command_permission_and_is_audited(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    drop(conn);
    for role in [Role::Viewer, Role::MaintenanceService] {
        let (browser, _) = scoped(&pool, &tenant, role, GrantScope::table(tenant.table)).await;
        assert_eq!(
            browser
                .request("POST", &format!("/api/v1/jobs/{job}/cancel"), None, None)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    let mut conn = pool.acquire().await.unwrap();
    let permissions: Vec<String> = sqlx::query_scalar(
        "SELECT detail->>'permission' FROM audit_events
         WHERE action = 'api.permission_denied' AND outcome = 'Denied' ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    assert_eq!(permissions, ["jobs:cancel", "jobs:cancel"]);
    let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = $1")
        .bind(job.as_uuid())
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(state, "Planning");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn retry_creates_a_fresh_linked_job_from_a_terminal_source(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    let succeeded = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, succeeded, JobState::Succeeded).await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    assert_eq!(
        admin
            .request(
                "POST",
                &format!("/api/v1/jobs/{job}/retry"),
                None,
                Some("first")
            )
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        admin
            .request(
                "POST",
                &format!("/api/v1/jobs/{succeeded}/retry"),
                None,
                Some("succeeded")
            )
            .await
            .0,
        StatusCode::CONFLICT
    );

    let mut conn = pool.acquire().await.unwrap();
    support::force_job_state(&mut conn, job, JobState::ReplanRequired).await;
    drop(conn);

    let (status, body) = admin
        .request(
            "POST",
            &format!("/api/v1/jobs/{job}/retry"),
            None,
            Some("first"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["retry_of_job_id"], job.to_string());
    let retry: Uuid = body["job_id"].as_str().unwrap().parse().unwrap();
    assert_ne!(retry, *job.as_uuid());
    let created = admin
        .request("GET", &format!("/api/v1/jobs/{retry}"), None, None)
        .await;
    assert_eq!(created.0, StatusCode::OK);
    assert_eq!(created.1["state"], "Planning");
    assert_eq!(created.1["retry_of_job_id"], job.to_string());
    assert_eq!(created.1["created_by"], tenant.user.to_string());

    // The same key and source return the original retry.
    assert_eq!(
        admin
            .request(
                "POST",
                &format!("/api/v1/jobs/{job}/retry"),
                None,
                Some("first")
            )
            .await,
        (status, body.clone())
    );
    // A fresh key creates another job; retry never reuses work.
    let second = admin
        .request(
            "POST",
            &format!("/api/v1/jobs/{job}/retry"),
            None,
            Some("second"),
        )
        .await;
    assert_eq!(second.0, StatusCode::ACCEPTED);
    assert_ne!(second.1["job_id"], body["job_id"]);
    assert_eq!(
        admin
            .request("POST", &format!("/api/v1/jobs/{job}/retry"), None, None)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM jobs WHERE retry_of_job_id = $1),
                (SELECT count(*) FROM audit_events WHERE action = 'job.retry_requested')",
    )
    .bind(job.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counts, (2, 2));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn retry_accepts_any_terminal_source_but_success(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let cancelled = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, cancelled, JobState::Cancelled).await;
    let no_op = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, no_op, JobState::NoOp).await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    for (source, key) in [(cancelled, "cancelled"), (no_op, "noop")] {
        let (status, body) = admin
            .request(
                "POST",
                &format!("/api/v1/jobs/{source}/retry"),
                None,
                Some(key),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(body["retry_of_job_id"], source.to_string());
    }
    let retries: i64 =
        sqlx::query_scalar("SELECT count(*) FROM jobs WHERE retry_of_job_id IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(retries, 2);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn concurrent_retries_share_one_operation(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, job, JobState::Failed).await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let path = format!("/api/v1/jobs/{job}/retry");
    let (left, right) = tokio::join!(
        admin.request("POST", &path, None, Some("same")),
        admin.request("POST", &path, None, Some("same"))
    );
    assert_eq!(left, right);
    assert_eq!(left.0, StatusCode::ACCEPTED);
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM jobs WHERE retry_of_job_id = $1),
                (SELECT count(*) FROM idempotency_keys WHERE operation = 'RetryJob')",
    )
    .bind(job.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counts, (1, 1));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn retry_is_denied_to_viewers_and_allowed_to_service_identities(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, job, JobState::Failed).await;
    drop(conn);
    let (viewer, _) = scoped(
        &pool,
        &tenant,
        Role::Viewer,
        GrantScope::table(tenant.table),
    )
    .await;
    assert_eq!(
        viewer
            .request(
                "POST",
                &format!("/api/v1/jobs/{job}/retry"),
                None,
                Some("viewer")
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let (service, _) = scoped(
        &pool,
        &tenant,
        Role::MaintenanceService,
        GrantScope::table(tenant.table),
    )
    .await;
    assert_eq!(
        service
            .request(
                "POST",
                &format!("/api/v1/jobs/{job}/retry"),
                None,
                Some("service")
            )
            .await
            .0,
        StatusCode::ACCEPTED
    );
    let denied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE action = 'api.permission_denied' AND detail->>'permission' = 'jobs:retry'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(denied, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn retry_and_cancel_require_the_csrf_header(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    support::force_job_state(&mut conn, job, JobState::Failed).await;
    drop(conn);
    let mut admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let csrf = std::mem::take(&mut admin.csrf);
    for (path, key) in [
        (format!("/api/v1/jobs/{job}/cancel"), None),
        (format!("/api/v1/jobs/{job}/retry"), Some("csrf")),
    ] {
        let (status, body) = admin.request("POST", &path, None, key).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["reason"], "csrf_rejected");
    }
    admin.csrf = csrf;
    assert_eq!(
        admin
            .request(
                "POST",
                &format!("/api/v1/jobs/{job}/retry"),
                None,
                Some("csrf")
            )
            .await
            .0,
        StatusCode::ACCEPTED
    );
}

// ---------------------------------------------------------------------------
// BS-018.T3: job event streams.
// ---------------------------------------------------------------------------

use axum::body::BodyDataStream;
use axum::response::Response;
use bergsmith_common::ids::JobId;
use bergsmith_persistence::cancellation::{self as cancellations, CancelJob};
use bergsmith_persistence::jobs::{self as job_ledger, Transition};
use futures::StreamExt;

/// The lifecycle every replay test drives a job through, after `Created`.
const LIFECYCLE: [JobState; 5] = [
    JobState::Queued,
    JobState::Running,
    JobState::ReadyToCommit,
    JobState::Committing,
    JobState::Succeeded,
];

/// Limits that never wake a stream on their own, so a test controls exactly
/// which mechanism delivers an event.
fn quiet_limits() -> EventStreamLimits {
    EventStreamLimits {
        poll_interval: Duration::from_secs(3_600),
        keep_alive: Duration::from_secs(3_600),
        recheck_interval: Duration::from_secs(3_600),
        ..EventStreamLimits::default()
    }
}

fn hub(limits: EventStreamLimits) -> JobEventHub {
    JobEventHub::new(limits).unwrap()
}

/// One parsed server-sent event.
#[derive(Debug)]
struct StreamedEvent {
    id: i64,
    event: String,
    data: Value,
}

/// Reads server-sent events from a response body, skipping comments.
struct EventReader {
    body: BodyDataStream,
    buffer: String,
}

impl EventReader {
    fn new(response: Response) -> Self {
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        assert_eq!(response.headers()["cache-control"], "no-store");
        Self {
            body: response.into_body().into_data_stream(),
            buffer: String::new(),
        }
    }

    /// The next event, or `None` once the server ends the stream.
    async fn next(&mut self) -> Option<StreamedEvent> {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                if let Some(event) = parse_event(&block) {
                    return Some(event);
                }
                continue;
            }
            // A timeout fails the test rather than hanging it.
            let chunk = tokio::time::timeout(Duration::from_secs(20), self.body.next())
                .await
                .unwrap()?;
            self.buffer
                .push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }
    }

    /// Every remaining event, requiring the server to end the stream.
    async fn rest(&mut self) -> Vec<StreamedEvent> {
        let mut events = Vec::new();
        while let Some(event) = self.next().await {
            events.push(event);
        }
        events
    }
}

fn parse_event(block: &str) -> Option<StreamedEvent> {
    let (mut id, mut event, mut data) = (None, None, None);
    for line in block.lines() {
        if let Some(value) = line.strip_prefix("id: ") {
            id = Some(value.parse().unwrap());
        } else if let Some(value) = line.strip_prefix("event: ") {
            event = Some(value.to_owned());
        } else if let Some(value) = line.strip_prefix("data: ") {
            data = Some(serde_json::from_str::<Value>(value).unwrap());
        }
    }
    let data = data?;
    let document: Value = serde_json::from_str(include_str!("../src/api/openapi.json")).unwrap();
    check_schema(
        &document,
        &document["components"]["schemas"]["JobEvent"],
        &data,
    );
    let id = id.unwrap();
    assert_eq!(data["sequence"], id);
    let event = event.unwrap();
    assert_eq!(data["kind"], event);
    Some(StreamedEvent { id, event, data })
}

async fn open_stream(browser: &Browser, uri: &str, last_event_id: Option<&str>) -> Response {
    let mut request = Request::builder()
        .uri(uri)
        .header("cookie", &browser.cookie)
        .header("accept", "text/event-stream");
    if let Some(last) = last_event_id {
        request = request.header("last-event-id", last);
    }
    browser
        .router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn advance(pool: &PgPool, job: JobId, expected_revision: i64, to: JobState) {
    let mut conn = pool.acquire().await.unwrap();
    job_ledger::transition_job(
        &mut conn,
        &Transition {
            job_id: job,
            expected_revision,
            to,
            reason: Some("TestStep".to_owned()),
            actor: None,
            non_publication_proven: false,
        },
    )
    .await
    .unwrap();
}

async fn finished_job(pool: &PgPool, tenant: &support::Tenant) -> JobId {
    let mut conn = pool.acquire().await.unwrap();
    let job = support::job(&mut conn, tenant).await;
    drop(conn);
    for (revision, state) in (0..).zip(LIFECYCLE) {
        advance(pool, job, revision, state).await;
    }
    job
}

fn ids(events: &[StreamedEvent]) -> Vec<i64> {
    events.iter().map(|event| event.id).collect()
}

/// Waits until the hub's listener has subscribed, observed through
/// PostgreSQL rather than a timed sleep.
async fn wait_for_listener(pool: &PgPool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let listening: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND query LIKE 'LISTEN %'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if listening > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_finished_job_replays_its_history_and_resumes_after_any_event(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let job = finished_job(&pool, &tenant).await;
    // A page smaller than the history exercises page boundaries.
    let admin = Browser::with_events(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        hub(EventStreamLimits {
            replay_page: 2,
            ..quiet_limits()
        }),
    )
    .await;
    let uri = format!("/api/v1/jobs/{job}/events");

    let all = EventReader::new(open_stream(&admin, &uri, None).await)
        .rest()
        .await;
    assert_eq!(ids(&all), [1, 2, 3, 4, 5, 6]);
    assert_eq!(all[0].event, "created");
    assert_eq!(all[0].data["to_state"], "Planning");
    assert!(all[0].data["from_state"].is_null());
    assert_eq!(all[0].data["actor_id"], tenant.user.to_string());
    let last = &all[5];
    assert_eq!(last.event, "transitioned");
    assert_eq!(last.data["from_state"], "Committing");
    assert_eq!(last.data["to_state"], "Succeeded");
    assert_eq!(last.data["state_revision"], 5);
    assert_eq!(last.data["reason"], "TestStep");

    let resumed = EventReader::new(open_stream(&admin, &uri, Some("3")).await)
        .rest()
        .await;
    assert_eq!(ids(&resumed), [4, 5, 6]);
    let queried = EventReader::new(open_stream(&admin, &format!("{uri}?after=4"), None).await)
        .rest()
        .await;
    assert_eq!(ids(&queried), [5, 6]);
    // The header a reconnecting EventSource sends wins over the query.
    let both = EventReader::new(open_stream(&admin, &format!("{uri}?after=1"), Some("5")).await)
        .rest()
        .await;
    assert_eq!(ids(&both), [6]);
    // Resuming at or past the end of a finished job ends immediately.
    for last in ["6", "60"] {
        assert!(
            EventReader::new(open_stream(&admin, &uri, Some(last)).await)
                .rest()
                .await
                .is_empty()
        );
    }
    let document: Value = serde_json::from_str(include_str!("../src/api/openapi.json")).unwrap();
    assert!(
        document["paths"]["/api/v1/jobs/{id}/events"]["get"]["responses"]["200"]["content"]
            ["text/event-stream"]
            .is_object()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn notifications_deliver_live_events_and_operator_reasons_stay_private(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    drop(conn);
    // Polling and rechecks never fire, so only a notification can deliver.
    let events = hub(quiet_limits());
    let listener = events.listen(&Database::from_pool(pool.clone()));
    wait_for_listener(&pool).await;
    let admin = Browser::with_events(&pool, &tenant, tenant.user, "alice", events).await;
    let mut reader =
        EventReader::new(open_stream(&admin, &format!("/api/v1/jobs/{job}/events"), None).await);
    assert_eq!(reader.next().await.unwrap().event, "created");

    let mut conn = pool.acquire().await.unwrap();
    cancellations::request_cancellation(
        &mut conn,
        &CancelJob {
            job_id: job,
            requested_by: Some(tenant.user),
            reason: Some("private operator note".to_owned()),
        },
    )
    .await
    .unwrap();
    drop(conn);

    let requested = reader.next().await.unwrap();
    assert_eq!(requested.id, 2);
    assert_eq!(requested.event, "cancellation_requested");
    assert!(requested.data["reason"].is_null());
    let transitioned = reader.next().await.unwrap();
    assert_eq!(transitioned.id, 3);
    assert_eq!(transitioned.data["to_state"], "Cancelling");
    assert_eq!(transitioned.data["reason"], "CancellationRequested");

    advance(&pool, job, 1, JobState::Cancelled).await;
    let ended = reader.rest().await;
    assert_eq!(ids(&ended), [4]);
    assert_eq!(ended[0].data["to_state"], "Cancelled");
    listener.abort();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn polling_delivers_events_when_no_notification_arrives(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    drop(conn);
    // No listener is started, as if every notification were lost.
    let admin = Browser::with_events(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        hub(EventStreamLimits {
            poll_interval: Duration::from_millis(20),
            ..quiet_limits()
        }),
    )
    .await;
    let mut reader =
        EventReader::new(open_stream(&admin, &format!("/api/v1/jobs/{job}/events"), None).await);
    assert_eq!(reader.next().await.unwrap().id, 1);
    advance(&pool, job, 0, JobState::Queued).await;
    let queued = reader.next().await.unwrap();
    assert_eq!(queued.id, 2);
    assert_eq!(queued.data["to_state"], "Queued");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_client_that_reconnects_during_concurrent_changes_sees_each_event_once(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    drop(conn);
    let events = hub(EventStreamLimits {
        poll_interval: Duration::from_millis(20),
        ..quiet_limits()
    });
    let listener = events.listen(&Database::from_pool(pool.clone()));
    let admin = Browser::with_events(&pool, &tenant, tenant.user, "alice", events).await;
    let uri = format!("/api/v1/jobs/{job}/events");

    let writer = tokio::spawn({
        let pool = pool.clone();
        async move {
            for (revision, state) in (0..).zip(LIFECYCLE) {
                advance(&pool, job, revision, state).await;
            }
        }
    });
    let mut first = EventReader::new(open_stream(&admin, &uri, None).await);
    let mut seen = vec![first.next().await.unwrap(), first.next().await.unwrap()];
    drop(first);
    let last = seen.last().unwrap().id.to_string();
    seen.extend(
        EventReader::new(open_stream(&admin, &uri, Some(&last)).await)
            .rest()
            .await,
    );
    writer.await.unwrap();

    assert_eq!(ids(&seen), [1, 2, 3, 4, 5, 6]);
    let revisions: Vec<_> = seen
        .iter()
        .map(|event| event.data["state_revision"].as_i64().unwrap())
        .collect();
    assert_eq!(revisions, [0, 1, 2, 3, 4, 5]);
    listener.abort();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn streams_end_after_session_revocation_grant_revocation_or_the_deadline(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    drop(conn);
    let uri = format!("/api/v1/jobs/{job}/events");
    let rechecking = EventStreamLimits {
        recheck_interval: Duration::from_millis(20),
        ..quiet_limits()
    };

    // Session revocation.
    let admin = Browser::with_events(&pool, &tenant, tenant.user, "alice", hub(rechecking)).await;
    let mut reader = EventReader::new(open_stream(&admin, &uri, None).await);
    assert_eq!(reader.next().await.unwrap().id, 1);
    sqlx::query("UPDATE auth_sessions SET revoked_at = now(), revoked_reason = 'logout'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(reader.rest().await.is_empty());

    // Grant revocation.
    let mut conn = pool.acquire().await.unwrap();
    let viewer = support::user(&mut conn, tenant.organization, "viewer").await;
    let grant = RoleGrantId::generate();
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: grant,
            organization_id: tenant.organization,
            principal_id: viewer,
            role: Role::Viewer,
            scope: GrantScope::table(tenant.table),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    drop(conn);
    let browser = Browser::with_events(&pool, &tenant, viewer, "viewer", hub(rechecking)).await;
    let mut reader = EventReader::new(open_stream(&browser, &uri, None).await);
    assert_eq!(reader.next().await.unwrap().id, 1);
    sqlx::query("UPDATE principal_role_grants SET revoked_at = now() WHERE id = $1")
        .bind(grant.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
    assert!(reader.rest().await.is_empty());

    // Read the initial event before explicitly advancing to the deadline.
    // A short wall-clock lifetime can expire during PostgreSQL replay under
    // coverage instrumentation, which correctly closes the stream with no data.
    let lifetime = Duration::from_secs(60);
    let bounded = Browser::with_events(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        hub(EventStreamLimits {
            max_duration: lifetime,
            ..quiet_limits()
        }),
    )
    .await;
    let mut reader = EventReader::new(open_stream(&bounded, &uri, None).await);
    assert_eq!(reader.next().await.unwrap().id, 1);
    tokio::time::pause();
    tokio::time::advance(lifetime).await;
    tokio::time::resume();
    assert!(reader.rest().await.is_empty());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn invalid_or_unauthorized_stream_requests_fail_before_streaming(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other = support::tenant(&mut conn, "other").await;
    let job = support::job(&mut conn, &tenant).await;
    let foreign = support::job(&mut conn, &other).await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let uri = format!("/api/v1/jobs/{job}/events");

    for query in ["after=-1", "after=x", "after=", "cursor=1"] {
        let (status, body) = admin
            .request("GET", &format!("{uri}?{query}"), None, None)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(body["reason"], "invalid_request");
    }
    let response = open_stream(&admin, &uri, Some("not-a-number")).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    for missing in [foreign, JobId::generate()] {
        let (status, body) = admin
            .request("GET", &format!("/api/v1/jobs/{missing}/events"), None, None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["reason"], "resource_not_found");
    }
    let (status, _, _) = raw(&admin.router, "GET", &uri).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn stream_capacity_is_bounded_per_principal_and_released_on_disconnect(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    drop(conn);
    let admin = Browser::with_events(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        hub(EventStreamLimits {
            max_streams_per_principal: 1,
            ..quiet_limits()
        }),
    )
    .await;
    let uri = format!("/api/v1/jobs/{job}/events");

    let held = open_stream(&admin, &uri, None).await;
    assert_eq!(held.status(), StatusCode::OK);
    let (status, body) = admin.request("GET", &uri, None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["reason"], "event_stream_capacity_exhausted");
    assert_eq!(body["disposition"], "RetrySameRequest");
    drop(held);
    assert_eq!(
        open_stream(&admin, &uri, None).await.status(),
        StatusCode::OK
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn buffered_events_stop_after_revocation_or_the_deadline(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    // One read buffers the whole history, so every later event comes from
    // the buffer rather than a fresh ledger read.
    let job = finished_job(&pool, &tenant).await;
    let uri = format!("/api/v1/jobs/{job}/events");

    // A recheck is due before every event.
    let admin = Browser::with_events(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        hub(EventStreamLimits {
            recheck_interval: Duration::from_nanos(1),
            ..quiet_limits()
        }),
    )
    .await;
    let mut reader = EventReader::new(open_stream(&admin, &uri, None).await);
    assert_eq!(reader.next().await.unwrap().id, 1);
    sqlx::query("UPDATE auth_sessions SET revoked_at = now(), revoked_reason = 'logout'")
        .execute(&pool)
        .await
        .unwrap();
    assert!(reader.rest().await.is_empty());

    // The deadline passes while the consumer holds unread buffered events.
    let lifetime = Duration::from_millis(500);
    let bounded = Browser::with_events(
        &pool,
        &tenant,
        tenant.user,
        "alice",
        hub(EventStreamLimits {
            max_duration: lifetime,
            ..quiet_limits()
        }),
    )
    .await;
    let mut reader = EventReader::new(open_stream(&bounded, &uri, None).await);
    assert_eq!(reader.next().await.unwrap().id, 1);
    // The behavior under test is time-based: outlive the stream's lifetime.
    tokio::time::sleep(lifetime).await;
    assert!(reader.rest().await.is_empty());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_listener_does_not_consume_the_request_pool(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    drop(conn);
    // A request pool with a single connection: a listener that kept a pooled
    // connection would leave every request waiting for this one.
    let small = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let events = hub(quiet_limits());
    let listener = events.listen(&Database::from_pool(small.clone()));
    wait_for_listener(&pool).await;
    let admin = Browser::with_events(&small, &tenant, tenant.user, "alice", events).await;

    let (status, body) = admin
        .request("GET", &format!("/api/v1/jobs/{job}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Notifications still arrive through the dedicated connection.
    let mut reader =
        EventReader::new(open_stream(&admin, &format!("/api/v1/jobs/{job}/events"), None).await);
    assert_eq!(reader.next().await.unwrap().id, 1);
    advance(&pool, job, 0, JobState::Queued).await;
    assert_eq!(reader.next().await.unwrap().id, 2);
    listener.abort();
}

/// A one-connection request pool whose acquire timeout far exceeds any
/// stream deadline used with it, so only the deadline can end a stalled wait.
async fn single_connection_pool(pool: &PgPool) -> PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(3_600))
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap()
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_deadline_ends_streams_whose_database_calls_stall(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let job = support::job(&mut conn, &tenant).await;
    drop(conn);
    let uri = format!("/api/v1/jobs/{job}/events");
    let lifetime = Duration::from_secs(1);

    for (stalled, limits) in [
        // A recheck is due before every event.
        (
            "recheck",
            EventStreamLimits {
                max_duration: lifetime,
                recheck_interval: Duration::from_nanos(1),
                ..quiet_limits()
            },
        ),
        // The next replay read follows a short poll.
        (
            "replay read",
            EventStreamLimits {
                max_duration: lifetime,
                poll_interval: Duration::from_millis(20),
                ..quiet_limits()
            },
        ),
    ] {
        let small = single_connection_pool(&pool).await;
        let admin = Browser::with_events(&small, &tenant, tenant.user, "alice", hub(limits)).await;
        let mut reader = EventReader::new(open_stream(&admin, &uri, None).await);
        assert_eq!(reader.next().await.unwrap().id, 1, "{stalled}");
        // Hold the only connection: the stream's next database call waits
        // indefinitely unless the deadline interrupts it.
        let held = small.acquire().await.unwrap();
        let started = std::time::Instant::now();
        assert!(reader.rest().await.is_empty(), "{stalled}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{stalled} wait outlived the deadline"
        );
        drop(held);
    }
}

async fn service_identity(
    pool: &PgPool,
    tenant: &support::Tenant,
    role: Role,
    scope: GrantScope,
) -> PrincipalId {
    let mut conn = pool.acquire().await.unwrap();
    let subject = RoleGrantId::generate().to_string();
    let id = support::service_principal(&mut conn, tenant.organization, &subject).await;
    grants::create_grant(
        &mut conn,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: tenant.organization,
            principal_id: id,
            role,
            scope,
            granted_by: Some(tenant.user),
        },
    )
    .await
    .unwrap();
    id
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn policies_and_schedules_are_audited_configuration_with_revisions(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let service = service_identity(
        &pool,
        &tenant,
        Role::MaintenanceService,
        GrantScope::table(tenant.table),
    )
    .await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, policy) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({
                "name": "nightly",
                "target_file_size_bytes": 536_870_912_u64,
                "min_input_files": 4,
                "max_input_bytes": 8_589_934_592_u64,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{policy}");
    assert_eq!(policy["operation"], "Compact");
    assert_eq!(policy["revision"], 1);
    assert_eq!(policy["enabled"], true);
    assert_eq!(policy["max_input_bytes"], json!(8_589_934_592_u64));
    let policy_id = policy["id"].as_str().unwrap().to_owned();

    let (status, listed) = browser.request("GET", "/api/v1/policies", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert_eq!(listed["next_cursor"], Value::Null);

    let (status, updated) = browser
        .request(
            "PATCH",
            &format!("/api/v1/policies/{policy_id}"),
            Some(json!({
                "revision": 1,
                "enabled": false,
                "min_input_files": 6,
                "max_input_bytes": 4_294_967_296_u64,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["revision"], 2);
    assert_eq!(updated["enabled"], false);
    assert_eq!(updated["min_input_files"], 6);
    assert_eq!(updated["max_input_bytes"], json!(4_294_967_296_u64));

    let (status, stale) = browser
        .request(
            "PATCH",
            &format!("/api/v1/policies/{policy_id}"),
            Some(json!({ "revision": 1, "enabled": true })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale["reason"], "revision_conflict");
    assert_eq!(stale["disposition"], "PermanentFailure");

    let (status, schedule) = browser
        .request(
            "POST",
            "/api/v1/schedules",
            Some(json!({
                "policy_id": policy_id,
                "table_id": tenant.table,
                "cron_expression": "30 2 * * MON-FRI",
                "time_zone": "Europe/Berlin",
                "window": { "start_minute": 120, "end_minute": 240 },
                "service_principal_id": service,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{schedule}");
    assert_eq!(schedule["cron_expression"], "30 2 * * MON-FRI");
    assert_eq!(schedule["time_zone"], "Europe/Berlin");
    assert_eq!(
        schedule["window"],
        json!({ "start_minute": 120, "end_minute": 240 })
    );
    assert_eq!(schedule["enabled"], true);
    assert_eq!(schedule["revision"], 1);
    let schedule_id = schedule["id"].as_str().unwrap().to_owned();

    let (status, read) = browser
        .request(
            "GET",
            &format!("/api/v1/schedules/{schedule_id}"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["id"], schedule["id"]);
    assert_eq!(read["policy_id"], policy_id);

    let (status, disabled) = browser
        .request(
            "PATCH",
            &format!("/api/v1/schedules/{schedule_id}"),
            Some(json!({ "revision": 1, "enabled": false })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{disabled}");
    assert_eq!(disabled["revision"], 2);
    assert_eq!(disabled["enabled"], false);
    // An update that does not name the window leaves it unchanged.
    assert_eq!(
        disabled["window"],
        json!({ "start_minute": 120, "end_minute": 240 })
    );

    // Every configuration change has an attributable audit record.
    let mut conn = pool.acquire().await.unwrap();
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_events
         WHERE organization_id = $1 AND resource_id IN ($2, $3)
         ORDER BY id",
    )
    .bind(tenant.organization.as_uuid())
    .bind(&policy_id)
    .bind(&schedule_id)
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        actions,
        vec![
            "policy.created",
            "policy.updated",
            "schedule.created",
            "schedule.updated"
        ]
    );

    // The update audit carries the persisted compaction parameters, so a
    // changed target size or input-file count is reconstructable afterwards.
    let detail: Value = sqlx::query_scalar(
        "SELECT detail FROM audit_events
         WHERE organization_id = $1 AND action = 'policy.updated'",
    )
    .bind(tenant.organization.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(detail["target_file_size_bytes"], json!(536_870_912_u64));
    assert_eq!(detail["min_input_files"], json!(6));
    assert_eq!(detail["max_input_bytes"], json!(4_294_967_296_u64));
    assert_eq!(detail["enabled"], json!(false));
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn scheduling_rejects_invalid_timing_and_ineligible_identities(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({ "name": "too-small", "target_file_size_bytes": 1 })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["reason"], "invalid_policy_parameters");

    // The per-run budget is bounded, and it must hold one replacement file.
    for max_input_bytes in [0, u64::MAX] {
        let (status, body) = browser
            .request(
                "POST",
                "/api/v1/policies",
                Some(json!({ "name": "bad-budget", "max_input_bytes": max_input_bytes })),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{max_input_bytes}");
        assert_eq!(body["reason"], "invalid_policy_parameters");
    }
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({
                "name": "too-tight-budget",
                "target_file_size_bytes": 536_870_912_u64,
                "max_input_bytes": 268_435_456_u64,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["reason"], "run_budget_below_target_file_size");

    // Raising the target above the stored run budget is rejected on update too,
    // and the failed update changes nothing.
    let (status, policy) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({
                "name": "bounded-update",
                "target_file_size_bytes": 1_048_576_u64,
                "max_input_bytes": 2_097_152_u64,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{policy}");
    let bounded_id = policy["id"].as_str().unwrap().to_owned();
    let (status, body) = browser
        .request(
            "PATCH",
            &format!("/api/v1/policies/{bounded_id}"),
            Some(json!({ "revision": 1, "target_file_size_bytes": 4_194_304_u64 })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["reason"], "run_budget_below_target_file_size");
    let (status, read) = browser
        .request("GET", &format!("/api/v1/policies/{bounded_id}"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["revision"], 1);
    assert_eq!(read["target_file_size_bytes"], json!(1_048_576_u64));
    assert_eq!(read["max_input_bytes"], json!(2_097_152_u64));

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({ "name": "" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["reason"], "invalid_request");

    let (status, policy) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({ "name": "nightly" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{policy}");
    let policy_id = policy["id"].as_str().unwrap().to_owned();

    for (expression, zone, reason) in [
        ("not a cron", "UTC", "invalid_cron_expression"),
        ("61 * * * *", "UTC", "invalid_cron_expression"),
        ("0 2 * * *", "Mars/Olympus_Mons", "invalid_time_zone"),
        ("0 9 * * *", "EST", "invalid_time_zone"),
    ] {
        let (status, body) = browser
            .request(
                "POST",
                "/api/v1/schedules",
                Some(json!({
                    "policy_id": policy_id,
                    "table_id": tenant.table,
                    "cron_expression": expression,
                    "time_zone": zone,
                    "service_principal_id": tenant.user,
                })),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{expression} {zone}");
        assert_eq!(body["reason"], reason);
    }

    // A person is never an execution identity, even with an administrator grant.
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/schedules",
            Some(json!({
                "policy_id": policy_id,
                "table_id": tenant.table,
                "cron_expression": "0 2 * * *",
                "time_zone": "UTC",
                "service_principal_id": tenant.user,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["reason"], "service_identity_not_eligible");

    // A policy cannot run twice against the same table.
    let service = service_identity(
        &pool,
        &tenant,
        Role::MaintenanceService,
        GrantScope::organization(),
    )
    .await;
    let (status, _) = browser
        .request(
            "POST",
            "/api/v1/schedules",
            Some(json!({
                "policy_id": policy_id,
                "table_id": tenant.table,
                "cron_expression": "0 2 * * *",
                "time_zone": "UTC",
                "service_principal_id": service,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/schedules",
            Some(json!({
                "policy_id": policy_id,
                "table_id": tenant.table,
                "cron_expression": "0 3 * * *",
                "time_zone": "UTC",
                "service_principal_id": service,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["reason"], "resource_already_exists");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn schedule_windows_are_validated_and_can_be_replaced_or_cleared(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let service = service_identity(
        &pool,
        &tenant,
        Role::MaintenanceService,
        GrantScope::table(tenant.table),
    )
    .await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, policy) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({ "name": "nightly" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{policy}");
    let policy_id = policy["id"].as_str().unwrap().to_owned();

    // 22:00–02:00 is one window that wraps past midnight.
    let (status, schedule) = browser
        .request(
            "POST",
            "/api/v1/schedules",
            Some(json!({
                "policy_id": policy_id,
                "table_id": tenant.table,
                "cron_expression": "0 23 * * *",
                "time_zone": "UTC",
                "window": { "start_minute": 1320, "end_minute": 120 },
                "service_principal_id": service,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{schedule}");
    assert_eq!(
        schedule["window"],
        json!({ "start_minute": 1320, "end_minute": 120 })
    );
    let schedule_id = schedule["id"].as_str().unwrap().to_owned();

    // A named window replaces the stored one.
    let (status, replaced) = browser
        .request(
            "PATCH",
            &format!("/api/v1/schedules/{schedule_id}"),
            Some(json!({
                "revision": 1,
                "window": { "start_minute": 60, "end_minute": 120 },
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["revision"], 2);
    assert_eq!(
        replaced["window"],
        json!({ "start_minute": 60, "end_minute": 120 })
    );

    // An explicit null clears it, while an absent field leaves it alone.
    let (status, cleared) = browser
        .request(
            "PATCH",
            &format!("/api/v1/schedules/{schedule_id}"),
            Some(json!({ "revision": 2, "window": null })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(cleared["revision"], 3);
    assert_eq!(cleared["window"], Value::Null);

    let (status, body) = browser
        .request(
            "PATCH",
            &format!("/api/v1/schedules/{schedule_id}"),
            Some(json!({ "revision": 3 })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["reason"], "invalid_request");

    // Empty and out-of-day windows are rejected with a window-specific reason.
    for window in [
        json!({ "start_minute": 60, "end_minute": 60 }),
        json!({ "start_minute": 1440, "end_minute": 60 }),
        json!({ "start_minute": 60, "end_minute": 1440 }),
        // Negative and wider-than-u16 values must reach the window validator
        // instead of failing as a generic malformed body.
        json!({ "start_minute": -1, "end_minute": 60 }),
        json!({ "start_minute": 60, "end_minute": 70_000 }),
    ] {
        let (status, body) = browser
            .request(
                "PATCH",
                &format!("/api/v1/schedules/{schedule_id}"),
                Some(json!({ "revision": 3, "window": window })),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{window}");
        assert_eq!(body["reason"], "invalid_maintenance_window");
    }
    let (status, body) = browser
        .request(
            "PATCH",
            &format!("/api/v1/schedules/{schedule_id}"),
            Some(json!({ "revision": 3, "window": { "start_minute": 60 } })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["reason"], "invalid_request");

    // A rejected window changes nothing, so revision 3 is still current.
    let (status, stale) = browser
        .request(
            "PATCH",
            &format!("/api/v1/schedules/{schedule_id}"),
            Some(json!({
                "revision": 1,
                "window": { "start_minute": 60, "end_minute": 120 },
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale["reason"], "revision_conflict");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn scheduling_configuration_requires_schedules_manage(pool: PgPool) {
    let tenant = {
        let mut conn = pool.acquire().await.unwrap();
        support::tenant(&mut conn, "acme").await
    };
    let (viewer, _) = scoped(&pool, &tenant, Role::Viewer, GrantScope::organization()).await;
    for (method, uri, body) in [
        ("GET", "/api/v1/policies".to_owned(), None),
        (
            "POST",
            "/api/v1/policies".to_owned(),
            Some(json!({ "name": "nightly" })),
        ),
        ("GET", "/api/v1/schedules".to_owned(), None),
        (
            "POST",
            "/api/v1/schedules".to_owned(),
            Some(json!({
                "policy_id": PolicyId::generate(),
                "table_id": tenant.table,
                "cron_expression": "0 2 * * *",
                "time_zone": "UTC",
                "service_principal_id": tenant.user,
            })),
        ),
    ] {
        let (status, body) = viewer.request(method, &uri, body, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(body["reason"], "permission_denied");
    }
    // Denials are audited even though no configuration transaction commits.
    let denials: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE organization_id = $1 AND action = 'api.permission_denied'",
    )
    .bind(tenant.organization.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(denials, 4);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn schedule_creation_rejects_an_unknown_policy(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let service = service_identity(
        &pool,
        &tenant,
        Role::MaintenanceService,
        GrantScope::organization(),
    )
    .await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/schedules",
            Some(json!({
                "policy_id": PolicyId::generate(),
                "table_id": tenant.table,
                "cron_expression": "0 2 * * *",
                "time_zone": "UTC",
                "service_principal_id": service,
            })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["reason"], "resource_not_found");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn table_scoped_administrators_cannot_manage_schedules(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other_table = tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(tenant.connection, "other"),
        Uuid::now_v7(),
    )
    .await
    .unwrap()
    .id;
    drop(conn);
    let service = service_identity(
        &pool,
        &tenant,
        Role::MaintenanceService,
        GrantScope::organization(),
    )
    .await;
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, policy) = admin
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({ "name": "nightly" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{policy}");
    let (status, schedule) = admin
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
    let schedule_id = schedule["id"].as_str().unwrap().to_owned();

    // Scheduling is organization configuration; a table-scoped administrator
    // cannot read or change a schedule even for the table they administer.
    let (scoped_admin, _) = scoped(
        &pool,
        &tenant,
        Role::Administrator,
        GrantScope::table(other_table),
    )
    .await;
    for (method, uri, body) in [
        ("GET", format!("/api/v1/schedules/{schedule_id}"), None),
        (
            "GET",
            format!("/api/v1/schedules/{schedule_id}/occurrences"),
            None,
        ),
        (
            "PATCH",
            format!("/api/v1/schedules/{schedule_id}"),
            Some(json!({ "revision": 1, "enabled": false })),
        ),
        (
            "POST",
            "/api/v1/schedules".to_owned(),
            Some(json!({
                "policy_id": policy["id"],
                "table_id": other_table,
                "cron_expression": "0 2 * * *",
                "time_zone": "UTC",
                "service_principal_id": service,
            })),
        ),
    ] {
        let (status, body) = scoped_admin.request(method, &uri, body, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(body["reason"], "permission_denied");
    }

    // Denials are attributable to the organization scope they failed.
    let denials: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE organization_id = $1 AND action = 'api.permission_denied'
           AND resource_type = 'organization' AND resource_id = $2",
    )
    .bind(tenant.organization.as_uuid())
    .bind(tenant.organization.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(denials, 4);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn policy_creation_rolls_back_when_the_audit_cannot_be_recorded(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    sqlx::query("CREATE FUNCTION reject_policy_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'test audit unavailable'; END $$").execute(&mut *conn).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_policy_audit BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_policy_audit()").execute(&mut *conn).await.unwrap();
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, body) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({ "name": "nightly" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["disposition"], "PermanentFailure");
    let policies: i64 = sqlx::query_scalar("SELECT count(*) FROM maintenance_policies")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(policies, 0);

    sqlx::query("DROP TRIGGER reject_policy_audit ON audit_events")
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({ "name": "nightly" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn policy_updates_reject_nonpositive_revisions(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let browser = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (status, policy) = browser
        .request(
            "POST",
            "/api/v1/policies",
            Some(json!({ "name": "nightly" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{policy}");
    let (status, body) = browser
        .request(
            "PATCH",
            &format!("/api/v1/policies/{}", policy["id"].as_str().unwrap()),
            Some(json!({ "revision": 0, "enabled": false })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["reason"], "invalid_request");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn worker_sessions_are_listed_for_administrators_only(pool: PgPool) {
    use bergsmith_common::ids::WorkerSessionId;
    use bergsmith_persistence::workers;

    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let other = support::tenant(&mut conn, "other").await;
    for (organization, name) in [
        (tenant.organization, "worker-1"),
        (other.organization, "worker-2"),
    ] {
        let principal = support::service_principal(&mut conn, organization, name).await;
        workers::register(
            &mut conn,
            &workers::Identity {
                organization,
                principal,
                pool: "native".into(),
            },
            &workers::Registration {
                id: WorkerSessionId::generate(),
                worker_name: name.into(),
                capabilities: vec!["native-v1".into()],
                memory_bytes: 2_147_483_648,
                cpu_millis: 1000,
                build_version: "0.1.0".into(),
            },
            Duration::from_secs(90),
        )
        .await
        .unwrap();
    }
    drop(conn);
    let administrator = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let (viewer, _) = scoped(&pool, &tenant, Role::Viewer, GrantScope::organization()).await;

    let (status, page) = administrator
        .request("GET", "/api/v1/workers", None, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["worker_name"], "worker-1");
    assert_eq!(items[0]["status"], "live");
    assert_eq!(items[0]["memory_bytes"], 2_147_483_648_i64);
    assert!(items[0]["running_attempt_id"].is_null());
    assert!(page["next_cursor"].is_null());

    let (status, denied) = viewer.request("GET", "/api/v1/workers", None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(denied.get("items").is_none());
}
