//! Browser resource contracts against the real identity and execution ledger.
#![expect(clippy::unwrap_used, reason = "route fixtures fail on setup errors")]

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bergsmith_common::ids::{PrincipalId, RoleGrantId};
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_coordinator::api::{self, ApiState};
use bergsmith_coordinator::identity::{
    CSRF_HEADER,
    session::{self, SessionConfig},
};
use bergsmith_persistence::Database;
use bergsmith_persistence::grants::{self, NewRoleGrant};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::BTreeSet;
use tower::ServiceExt;

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
    let template = match path.rsplit_once('/') {
        Some((parent, id)) if uuid::Uuid::parse_str(id).is_ok() => format!("{parent}/{{id}}"),
        _ => path.to_owned(),
    };
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
                    uuid::Uuid::parse_str(value).unwrap();
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
    let table_uuid: uuid::Uuid =
        sqlx::query_scalar("SELECT table_uuid FROM table_identities WHERE id = $1")
            .bind(tenant.table.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    bergsmith_persistence::tables::resolve_table(
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
        let (status, detail) = browser
            .request("GET", &format!("/api/v1/{resource}/{id}"), None, None)
            .await;
        assert_eq!(status, StatusCode::OK);
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
        if resource == "tables" {
            assert_eq!(body["items"][0]["aliases"].as_array().unwrap().len(), 1);
            assert_eq!(
                body["items"][0]["aliases"][0]["catalog_id"],
                tenant.connection.to_string()
            );
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
async fn future_resources_report_unavailability_without_claiming_execution(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    drop(conn);
    let admin = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    for (method, path) in [
        ("POST", "previews"),
        ("GET", "schedules"),
        ("GET", "policies"),
    ] {
        let result = admin
            .request(method, &format!("/api/v1/{path}"), None, None)
            .await;
        assert_eq!(result.0, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(result.1["disposition"], "PermanentFailure");
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
    let table = bergsmith_persistence::tables::resolve_table(
        &mut conn,
        tenant.organization,
        &support::alias(tenant.connection, "second"),
        uuid::Uuid::now_v7(),
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
    let id: bergsmith_common::ids::JobId = original.1["job_id"].as_str().unwrap().parse().unwrap();
    let mut conn = pool.acquire().await.unwrap();
    support::force_job_state(&mut conn, id, bergsmith_common::job::JobState::NoOp).await;
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
            .request("POST", "/api/v1/previews", None, None)
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
            .request("POST", "/api/v1/previews", None, None)
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
        ("/api/v1/catalogs/{id}", "get"),
        ("/api/v1/tables", "get"),
        ("/api/v1/tables/{id}", "get"),
        ("/api/v1/jobs", "get"),
        ("/api/v1/jobs", "post"),
        ("/api/v1/jobs/{id}", "get"),
        ("/api/v1/audit", "get"),
        ("/api/v1/previews", "post"),
        ("/api/v1/schedules", "get"),
        ("/api/v1/policies", "get"),
        ("/api/v1/openapi.json", "get"),
    ];
    let document: Value = serde_json::from_str(include_str!("../src/api/openapi.json")).unwrap();
    let documented: BTreeSet<(&str, &str)> = document["paths"]
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(path, operations)| {
            ["get", "post"].into_iter().filter_map(move |method| {
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
