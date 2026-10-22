//! Bootstrap commands for a new deployment (BS-020.T1).

#![expect(
    clippy::expect_used,
    reason = "test setup helpers fail the test immediately"
)]

use bergsmith_common::ids::TableId;
use bergsmith_coordinator::database::{self, DatabaseUrl};
use sqlx::PgPool;

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
async fn an_organization_connection_and_service_can_be_provisioned(pool: PgPool) {
    let url = url_for(&pool);
    database::migrate(url.clone()).await.unwrap();
    let organization = database::create_organization(url.clone(), "acme", "Acme")
        .await
        .unwrap();
    assert_eq!(organization.slug, "acme");

    let connection = database::add_catalog_connection(
        url.clone(),
        "acme",
        "lakekeeper",
        "http://127.0.0.1:58181",
        None,
    )
    .await
    .unwrap();
    assert_eq!(connection.name, "lakekeeper");

    let principal = database::bootstrap_service(url.clone(), "acme", "native-workers", None, None)
        .await
        .unwrap();
    assert_eq!(
        principal.organization_id.as_uuid(),
        organization.id.as_uuid()
    );

    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM organizations),
                (SELECT count(*) FROM catalog_connections),
                (SELECT count(*) FROM principals)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counts, (1, 1, 1));
}

#[sqlx::test(migrations = false)]
async fn a_schedule_identity_receives_a_table_scoped_role(pool: PgPool) {
    let url = url_for(&pool);
    database::migrate(url.clone()).await.unwrap();
    let organization = database::create_organization(url.clone(), "acme", "Acme")
        .await
        .unwrap();
    let table = TableId::generate();
    sqlx::query(
        "INSERT INTO table_identities (id, organization_id, table_uuid) VALUES ($1, $2, $3)",
    )
    .bind(table.as_uuid())
    .bind(organization.id.as_uuid())
    .bind(uuid::Uuid::now_v7())
    .execute(&pool)
    .await
    .unwrap();
    let principal = database::bootstrap_service(
        url.clone(),
        "acme",
        "scheduler",
        Some(bergsmith_common::permission::Role::MaintenanceService),
        Some(table),
    )
    .await
    .unwrap();
    let grant: (String, Option<uuid::Uuid>) = sqlx::query_as(
        "SELECT role, table_identity_id FROM principal_role_grants WHERE principal_id = $1",
    )
    .bind(principal.id.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(grant.0, "MaintenanceService");
    assert_eq!(grant.1, Some(*table.as_uuid()));
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_events ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(actions, ["identity.provisioned", "grant.created"]);
}

#[sqlx::test(migrations = false)]
async fn a_failed_service_grant_rolls_back_the_principal_and_audit(pool: PgPool) {
    let url = url_for(&pool);
    database::migrate(url.clone()).await.unwrap();
    database::create_organization(url.clone(), "acme", "Acme")
        .await
        .unwrap();
    assert!(
        database::bootstrap_service(
            url,
            "acme",
            "scheduler",
            Some(bergsmith_common::permission::Role::MaintenanceService),
            Some(TableId::generate()),
        )
        .await
        .is_err()
    );
    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM principals),
                (SELECT count(*) FROM principal_role_grants),
                (SELECT count(*) FROM audit_events)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counts, (0, 0, 0));
}

#[sqlx::test(migrations = false)]
async fn a_failed_service_audit_rolls_back_provisioning(pool: PgPool) {
    let url = url_for(&pool);
    database::migrate(url.clone()).await.unwrap();
    database::create_organization(url.clone(), "acme", "Acme")
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE FUNCTION reject_test_audit() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'audit unavailable'; END $$;
        CREATE TRIGGER reject_test_audit BEFORE INSERT ON audit_events
        FOR EACH ROW EXECUTE FUNCTION reject_test_audit();",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        database::bootstrap_service(url, "acme", "worker", None, None)
            .await
            .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM principals")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test(migrations = false)]
async fn a_duplicate_slug_is_rejected(pool: PgPool) {
    let url = url_for(&pool);
    database::migrate(url.clone()).await.unwrap();
    database::create_organization(url.clone(), "acme", "Acme")
        .await
        .unwrap();
    assert!(
        database::create_organization(url, "acme", "Other")
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = false)]
async fn database_settings_reject_a_duplicate_connection_name(pool: PgPool) {
    let url = url_for(&pool);
    database::migrate(url.clone()).await.unwrap();
    database::create_organization(url.clone(), "acme", "Acme")
        .await
        .unwrap();
    let uri = "http://127.0.0.1:58181";
    database::add_catalog_connection(url.clone(), "acme", "one", uri, None)
        .await
        .unwrap();
    assert!(
        database::add_catalog_connection(url, "acme", "one", uri, None)
            .await
            .is_err()
    );
}

#[test]
fn a_token_verifier_matches_the_protocol() {
    let token = "bsw1_native_abcdefghijklmnopqrstuvwxyz_ABCDEFGHIJKLMN-0";
    let verifier = database::service_token_verifier(token).unwrap();
    let expected = bergsmith_protocol::identity::ServiceToken::parse(token)
        .unwrap()
        .verifier()
        .to_hex();
    assert_eq!(verifier, expected);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn development_bootstrap_replay_repairs_the_catalog_warehouse(pool: PgPool) {
    let bootstrap = include_str!("../../../deploy/docker/bootstrap.sql");
    sqlx::raw_sql(bootstrap).execute(&pool).await.unwrap();
    sqlx::query("UPDATE catalog_connections SET warehouse = NULL")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(bootstrap).execute(&pool).await.unwrap();
    let warehouses: Vec<String> = sqlx::query_scalar("SELECT warehouse FROM catalog_connections")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(warehouses, ["development"]);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM principals")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn a_malformed_token_is_rejected() {
    assert!(database::service_token_verifier("not-a-token").is_err());
}
