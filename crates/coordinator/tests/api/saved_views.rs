//! Saved list views (BS-033.T6).
//!
//! Views belong to one person: another person's view reads as missing, a
//! save is idempotent, edits name their revision, parameters are checked
//! against the list, and every write is audited.

use super::*;

fn jobs_view(name: &str) -> Value {
    json!({
        "page": "jobs",
        "name": name,
        "params": { "state": "Failed", "order": "oldest" },
    })
}

async fn save(browser: &Browser, key: &str, body: Value) -> (StatusCode, Value) {
    browser
        .request("POST", "/api/v1/me/views", Some(body), Some(key))
        .await
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_view_is_saved_once_listed_edited_and_deleted_by_revision(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let alice = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    let (status, saved) = save(&alice, "save-1", jobs_view("  Failed  ")).await;
    assert_eq!(status, StatusCode::CREATED, "{saved}");
    assert_eq!(saved["name"], "Failed");
    assert_eq!(saved["revision"], 1);
    // The same key and request return the same view.
    let (status, replay) = save(&alice, "save-1", jobs_view("  Failed  ")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(replay["id"], saved["id"]);
    // The same key for another request is refused.
    let (status, reused) = save(&alice, "save-1", jobs_view("Other")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(reused["reason"], "idempotency_key_reused");

    let (_, listed) = alice
        .request("GET", "/api/v1/me/views?page=jobs", None, None)
        .await;
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    let (_, tables) = alice
        .request("GET", "/api/v1/me/views?page=tables", None, None)
        .await;
    assert_eq!(tables["items"], json!([]));

    let id = saved["id"].as_str().unwrap();
    let uri = format!("/api/v1/me/views/{id}");
    let (status, renamed) = alice
        .request(
            "PATCH",
            &uri,
            Some(json!({ "revision": 1, "name": "Failed jobs" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["revision"], 2);
    assert_eq!(renamed["params"]["state"], "Failed");
    // A stale revision changes nothing.
    let (status, stale) = alice
        .request(
            "PATCH",
            &uri,
            Some(json!({ "revision": 1, "params": { "q": "orders" } })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stale["reason"], "revision_conflict");
    let (status, _) = alice
        .request("DELETE", &format!("{uri}?revision=1"), None, None)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, deleted) = alice
        .request("DELETE", &format!("{uri}?revision=2"), None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["name"], "Failed jobs");
    let (status, _) = alice
        .request("DELETE", &format!("{uri}?revision=2"), None, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_events
         WHERE resource_type = 'saved_view' AND resource_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        actions,
        [
            "saved_view.created",
            "saved_view.updated",
            "saved_view.deleted"
        ]
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn another_person_never_sees_or_changes_a_view(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let bob = support::user(&mut conn, tenant.organization, "bob").await;
    let alice = Browser::new(&pool, &tenant, tenant.user, "alice").await;
    let bob = Browser::new(&pool, &tenant, bob, "bob").await;

    let (_, saved) = save(&alice, "alice-1", jobs_view("Mine")).await;
    let id = saved["id"].as_str().unwrap();

    let (_, listed) = bob.request("GET", "/api/v1/me/views", None, None).await;
    assert_eq!(listed["items"], json!([]));
    let (status, _) = bob
        .request(
            "PATCH",
            &format!("/api/v1/me/views/{id}"),
            Some(json!({ "revision": 1, "name": "Taken" })),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = bob
        .request(
            "DELETE",
            &format!("/api/v1/me/views/{id}?revision=1"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Keys are scoped to the organization: another person's request under
    // the same key is refused without revealing the first person's view.
    let (status, reused) = save(&bob, "alice-1", jobs_view("Mine")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(reused["reason"], "idempotency_key_reused");
    assert!(reused.get("id").is_none());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn views_are_checked_against_their_list_and_bounded(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let tenant = support::tenant(&mut conn, "acme").await;
    let alice = Browser::new(&pool, &tenant, tenant.user, "alice").await;

    for (key, body) in [
        (
            "p1",
            json!({ "page": "jobs", "name": "x", "params": { "health": "critical" } }),
        ),
        ("p2", json!({ "page": "nope", "name": "x", "params": {} })),
        ("p3", json!({ "page": "jobs", "name": " ", "params": {} })),
        (
            "p4",
            json!({ "page": "jobs", "name": "x", "params": { "q": "" } }),
        ),
        (
            "p5",
            json!({ "page": "jobs", "name": "x", "params": { "q": "a\u{1}b" } }),
        ),
    ] {
        let (status, body) = save(&alice, key, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{key}: {body}");
    }
    let (status, _) = alice
        .request("POST", "/api/v1/me/views", Some(jobs_view("No key")), None)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = save(&alice, "dup-1", jobs_view("Same")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, duplicate) = save(&alice, "dup-2", jobs_view("Same")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(duplicate["reason"], "resource_already_exists");
    // The same name on another list is fine.
    let (status, _) = save(
        &alice,
        "dup-3",
        json!({ "page": "tables", "name": "Same", "params": { "health": "critical" } }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    for index in 2..50 {
        let (status, body) = save(
            &alice,
            &format!("n-{index}"),
            jobs_view(&format!("v{index}")),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{index}: {body}");
    }
    let (status, full) = save(&alice, "n-50", jobs_view("one too many")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(full["reason"], "view_limit_reached");
}
