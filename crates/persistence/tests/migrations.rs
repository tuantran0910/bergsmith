//! Migration and connection tests against a real PostgreSQL server.
//!
//! `#[sqlx::test]` creates a fresh database per test on the server named by
//! `DATABASE_URL`; `make test` starts one with Docker Compose.

use std::time::Duration;

use bergsmith_persistence::{Database, DatabaseConfig, DatabaseUrl, PersistenceError};
use sqlx::PgPool;

#[sqlx::test(migrations = false)]
async fn connection_management_migration_preserves_existing_connections_as_enabled(pool: PgPool) {
    let earlier = sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(
            bergsmith_persistence::MIGRATOR
                .iter()
                .filter(|migration| migration.version < 17)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    earlier.run(&pool).await.unwrap();
    let organization = uuid::Uuid::now_v7();
    let connection = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id,slug,display_name) VALUES ($1,'acme','Acme')")
        .bind(organization)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO catalog_connections (id,organization_id,name,uri) VALUES ($1,$2,'legacy','https://catalog.example/api')")
        .bind(connection).bind(organization).execute(&pool).await.unwrap();
    bergsmith_persistence::MIGRATOR.run(&pool).await.unwrap();
    let stored: (bool, i64, String) =
        sqlx::query_as("SELECT enabled,revision,uri FROM catalog_connections WHERE id=$1")
            .bind(connection)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, (true, 0, "https://catalog.example/api".to_owned()));
}

#[sqlx::test(migrations = false)]
async fn migrations_apply_and_reapply_without_changes(pool: PgPool) {
    let database = Database::from_pool(pool);

    database.migrate().await.unwrap();
    database.migrate().await.unwrap();

    let applied: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations WHERE success")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(
        applied,
        i64::try_from(bergsmith_persistence::MIGRATOR.iter().count()).unwrap()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn reject_mutation_trigger_blocks_updates_and_deletes(pool: PgPool) {
    sqlx::raw_sql(
        "CREATE TABLE frozen (id int PRIMARY KEY);
         INSERT INTO frozen VALUES (1);
         CREATE TRIGGER frozen_immutable BEFORE UPDATE OR DELETE ON frozen
             FOR EACH ROW EXECUTE FUNCTION bergsmith_reject_mutation();",
    )
    .execute(&pool)
    .await
    .unwrap();

    for statement in ["UPDATE frozen SET id = 2", "DELETE FROM frozen"] {
        let error = sqlx::raw_sql(statement).execute(&pool).await.unwrap_err();
        let code = error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .unwrap();
        assert_eq!(code, "23001", "{statement} should violate restrict");
    }
}

#[tokio::test]
async fn connect_reaches_the_configured_server() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is set by the test harness");
    let database = Database::connect(&DatabaseConfig::new(DatabaseUrl::new(url)))
        .await
        .unwrap();

    let one: i32 = sqlx::query_scalar("SELECT 1")
        .fetch_one(database.pool())
        .await
        .unwrap();
    assert_eq!(one, 1);
}

#[tokio::test]
async fn connect_fails_within_the_acquire_timeout_when_unreachable() {
    let mut config = DatabaseConfig::new(DatabaseUrl::new("postgres://bergsmith@127.0.0.1:9/db"));
    config.acquire_timeout = Duration::from_millis(500);

    let error = Database::connect(&config).await.unwrap_err();

    assert!(matches!(error, PersistenceError::Database(_)));
}
