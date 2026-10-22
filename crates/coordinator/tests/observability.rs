//! Metrics exposition against a real ledger (BS-020.T2).

#![allow(clippy::unwrap_used, reason = "fixtures fail the test immediately")]

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bergsmith_coordinator::observability::{self, MetricsConfig, Observability};
use bergsmith_persistence::Database;
use sqlx::PgPool;
use tower::ServiceExt as _;

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn metrics_are_served_from_a_cached_ledger_snapshot(pool: PgPool) {
    let observability = Observability::new(
        Database::from_pool(pool.clone()),
        MetricsConfig {
            refresh: Duration::from_secs(3600),
            query_timeout: Duration::from_secs(5),
        },
    );
    let router = observability::router(observability.clone());

    let response = router
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/plain; version=0.0.4")
    );
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("bergsmith_ledger_up 1\n"));
    assert!(text.contains("bergsmith_jobs_active{state=\"Queued\"} 0\n"));
    assert!(text.contains("bergsmith_tasks_pending 0\n"));

    // Within the refresh interval the cached snapshot is served, so a closed
    // pool does not fail the scrape.
    pool.close().await;
    let cached = observability.render().await;
    assert!(cached.contains("bergsmith_ledger_up 1\n"));
    assert!(cached.contains("bergsmith_tasks_pending 0\n"));
}
