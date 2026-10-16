//! Tests for the `migrate` administration command.

// clippy's `allow-expect-in-tests` covers only `#[test]` functions; the shared
// setup helper below should also fail the test on any error.
#![expect(clippy::expect_used, reason = "test setup helpers fail the test")]

use bergsmith_coordinator::database::{self, DatabaseUrl};
use sqlx::PgPool;

/// Returns `DATABASE_URL` pointed at the per-test database behind `pool`.
fn url_for(pool: &PgPool) -> DatabaseUrl {
    let server = std::env::var("DATABASE_URL").expect("DATABASE_URL is set by the test harness");
    let (base, _) = server
        .rsplit_once('/')
        .expect("DATABASE_URL names a database");
    let name = pool
        .connect_options()
        .get_database()
        .expect("test pools name their database")
        .to_owned();
    DatabaseUrl::new(format!("{base}/{name}"))
}

#[sqlx::test(migrations = false)]
async fn migrate_applies_the_schema_and_is_idempotent(pool: PgPool) {
    database::migrate(url_for(&pool)).await.unwrap();
    database::migrate(url_for(&pool)).await.unwrap();

    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(applied > 0);
}
