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
use bergsmith_coordinator::preview::{CatalogPreviewer, PreviewLimits, PreviewProfiles};
use bergsmith_maintenance::PlannerConfig;
use bergsmith_persistence::Database;
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_storage::{S3Profile, StorageProfile, StorageProfiles};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::BTreeSet;
use tower::ServiceExt;

/// A previewer over the local filesystem, which is where fixtures live.
fn previewer() -> CatalogPreviewer {
    CatalogPreviewer::new(
        PreviewProfiles::new(StorageProfiles::new([StorageProfile::LocalFs])),
        PlannerConfig::default(),
    )
    .unwrap()
}

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
    let template = path
        .split('/')
        .map(|segment| {
            if Uuid::parse_str(segment).is_ok() {
                "{id}"
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
    for (method, path) in [("GET", "schedules"), ("GET", "policies")] {
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
    let id: bergsmith_common::ids::JobId = original.1["job_id"].as_str().unwrap().parse().unwrap();
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
        ("/api/v1/catalogs/{id}", "get"),
        ("/api/v1/tables", "get"),
        ("/api/v1/tables/{id}", "get"),
        ("/api/v1/jobs", "get"),
        ("/api/v1/jobs", "post"),
        ("/api/v1/jobs/{id}", "get"),
        ("/api/v1/jobs/{id}/cancel", "post"),
        ("/api/v1/jobs/{id}/retry", "post"),
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
        body["baseline"]["snapshot_id"].as_i64(),
        fixture.metadata().current_snapshot_id()
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
