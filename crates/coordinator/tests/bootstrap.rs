//! Bootstrap commands for a new deployment (BS-020.T1).

#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test setup helpers fail the test immediately"
)]

use bergsmith_common::ids::{OrganizationId, PrincipalId, TableId, WorkerPoolTokenId};
use bergsmith_coordinator::database::{self, DatabaseUrl};
use bergsmith_coordinator::identity::administration::{self, TokenDeliveryError};
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

#[sqlx::test(migrations = false)]
async fn a_worker_token_is_issued_once_for_a_service_identity(pool: PgPool) {
    let url = url_for(&pool);
    database::migrate(url.clone()).await.unwrap();
    database::create_organization(url.clone(), "acme", "Acme")
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let administrator = database::bootstrap_administrator(
        &mut conn,
        &database::BootstrapAdministrator {
            organization: "acme".to_owned(),
            issuer: "https://idp.example".to_owned(),
            subject: "alice".to_owned(),
            display_name: "Alice".to_owned(),
        },
    )
    .await
    .unwrap();
    drop(conn);
    let service = database::bootstrap_service(url.clone(), "acme", "native-workers", None, None)
        .await
        .unwrap();

    let issued = database::issue_worker_token(
        url.clone(),
        "acme",
        administrator,
        service.id,
        "native",
        Some("worker-1"),
    )
    .await
    .unwrap();
    let token = bergsmith_protocol::identity::ServiceToken::parse(issued.secret()).unwrap();
    assert_eq!(token.pool(), "native");
    assert_eq!(issued.token.label.as_deref(), Some("worker-1"));
    let stored: (Vec<u8>, i64) = sqlx::query_as(
        "SELECT verifier, (SELECT count(*) FROM audit_events
                            WHERE action = 'worker_token.issued')
         FROM worker_pool_tokens WHERE id = $1",
    )
    .bind(issued.token.id.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.0, token.verifier().as_bytes().to_vec());
    assert_eq!(stored.1, 1);

    // A service identity cannot issue tokens for itself; workers:manage is
    // required and service identities hold only their granted roles.
    assert!(
        database::issue_worker_token(url, "acme", service.id, service.id, "native", None)
            .await
            .is_err()
    );
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

struct FailingWriter;

impl std::io::Write for FailingWriter {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("simulated disk full"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn issuance_fixture(
    pool: &PgPool,
) -> (DatabaseUrl, OrganizationId, PrincipalId, PrincipalId) {
    let url = url_for(pool);
    database::migrate(url.clone()).await.unwrap();
    let organization = database::create_organization(url.clone(), "acme", "Acme")
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let administrator = database::bootstrap_administrator(
        &mut conn,
        &database::BootstrapAdministrator {
            organization: "acme".to_owned(),
            issuer: "https://idp.example".to_owned(),
            subject: "alice".to_owned(),
            display_name: "Alice".to_owned(),
        },
    )
    .await
    .unwrap();
    drop(conn);
    let service = database::bootstrap_service(url.clone(), "acme", "native-workers", None, None)
        .await
        .unwrap();
    (url, organization.id, administrator, service.id)
}

#[sqlx::test(migrations = false)]
async fn an_existing_token_file_is_refused_before_issuance(pool: PgPool) {
    let (url, organization, actor, principal) = issuance_fixture(&pool).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("worker-token");
    std::fs::write(&path, "occupied\n").unwrap();

    let error =
        database::issue_worker_token_to_file(url, "acme", actor, principal, "native", None, &path)
            .await
            .unwrap_err();
    assert!(
        matches!(
            error,
            database::IssueWorkerTokenError::Delivery(TokenDeliveryError::File { .. })
        ),
        "{error:?}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "occupied\n");
    let tokens: i64 =
        sqlx::query_scalar("SELECT count(*) FROM worker_pool_tokens WHERE organization_id = $1")
            .bind(organization.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(tokens, 0, "no token is issued when the file is occupied");
    let issued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_events
         WHERE organization_id = $1 AND action = 'worker_token.issued'",
    )
    .bind(organization.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(issued, 0, "no issuance is recorded");
}

#[sqlx::test(migrations = false)]
async fn a_failed_write_revokes_the_undelivered_token(pool: PgPool) {
    let (_, organization, actor, principal) = issuance_fixture(&pool).await;
    let mut conn = pool.acquire().await.unwrap();
    let error = administration::issue_worker_token_to_writer(
        &mut conn,
        organization,
        actor,
        principal,
        "native",
        None,
        &mut FailingWriter,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, TokenDeliveryError::Revoked { .. }),
        "expected a revoked undelivered token, got {error:?}"
    );
    let token = match error {
        TokenDeliveryError::Revoked { token, .. } => token,
        _ => WorkerPoolTokenId::generate(),
    };
    drop(conn);

    let (revoked, audit): (bool, i64) = sqlx::query_as(
        "SELECT t.revoked_at IS NOT NULL,
                (SELECT count(*) FROM audit_events a
                 WHERE a.action = 'worker_token.revoked' AND a.resource_id = $2)
         FROM worker_pool_tokens t WHERE t.id = $1",
    )
    .bind(token.as_uuid())
    .bind(token.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(revoked, "the undelivered token is not active");
    assert_eq!(audit, 1, "the cleanup revocation is audited");
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM worker_pool_tokens
         WHERE organization_id = $1 AND revoked_at IS NULL",
    )
    .bind(organization.as_uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, 0);
}

#[sqlx::test(migrations = false)]
async fn a_worker_token_file_contains_only_the_bearer_token(pool: PgPool) {
    let (url, _, actor, principal) = issuance_fixture(&pool).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("worker-token");
    let issued = database::issue_worker_token_to_file(
        url,
        "acme",
        actor,
        principal,
        "native",
        Some("worker-1"),
        &path,
    )
    .await
    .unwrap();

    let stored = std::fs::read_to_string(&path).unwrap();
    let token = bergsmith_protocol::identity::ServiceToken::parse(stored.trim()).unwrap();
    assert_eq!(token.expose_secret(), issued.secret());
    assert_eq!(issued.token.pool, "native");
    assert!(
        !format!("{issued:?}").contains(issued.secret()),
        "debug output must not expose the bearer token"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
