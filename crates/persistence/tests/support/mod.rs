//! Fixtures shared by the persistence integration tests.
//!
//! Each test file includes this module with `mod support;`, so helpers unused
//! by one file are not dead code in general.
#![allow(dead_code, reason = "each test binary uses a different subset")]
#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "fixtures fail the test on setup errors"
)]

use std::time::Duration;

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{
    AttemptId, CatalogConnectionId, JobId, OrganizationId, PlanId, PrincipalId, TableId, TaskId,
    WorkerPoolTokenId, WorkerSessionId,
};
use bergsmith_common::job::JobState;
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::attempts::{self, Attempt, NewGrant};
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::jobs::{self, NewJob};
use bergsmith_persistence::plans::{self, NewPlan};
use bergsmith_persistence::tables::{self, TableAlias};
use bergsmith_persistence::tenancy::{
    self, NewCatalogConnection, NewOrganization, NewPrincipal, PrincipalKind,
};
use sqlx::PgConnection;
use uuid::Uuid;

/// An organization with one user, one catalog connection, and one table.
pub(crate) struct Tenant {
    pub(crate) organization: OrganizationId,
    pub(crate) user: PrincipalId,
    pub(crate) connection: CatalogConnectionId,
    pub(crate) table: TableId,
}

/// Creates an organization named `slug`.
pub(crate) async fn organization(conn: &mut PgConnection, slug: &str) -> OrganizationId {
    let id = OrganizationId::generate();
    tenancy::create_organization(
        conn,
        &NewOrganization {
            id,
            slug: slug.to_owned(),
            display_name: slug.to_owned(),
        },
    )
    .await
    .expect("organization is created");
    id
}

/// Creates a user principal with `subject` in `organization`.
pub(crate) async fn user(
    conn: &mut PgConnection,
    organization: OrganizationId,
    subject: &str,
) -> PrincipalId {
    let id = PrincipalId::generate();
    tenancy::create_principal(
        conn,
        &NewPrincipal {
            id,
            organization_id: organization,
            kind: PrincipalKind::User,
            issuer: "https://idp.example".to_owned(),
            subject: subject.to_owned(),
            display_name: subject.to_owned(),
        },
    )
    .await
    .expect("principal is created");
    id
}

/// Creates a service principal with `subject` in `organization`.
pub(crate) async fn service_principal(
    conn: &mut PgConnection,
    organization: OrganizationId,
    subject: &str,
) -> PrincipalId {
    let id = PrincipalId::generate();
    tenancy::create_principal(
        conn,
        &NewPrincipal {
            id,
            organization_id: organization,
            kind: PrincipalKind::Service,
            issuer: "bergsmith:test".to_owned(),
            subject: subject.to_owned(),
            display_name: subject.to_owned(),
        },
    )
    .await
    .expect("service principal is created");
    id
}

/// Registers a worker pool token verifier for `principal`, idempotently.
///
/// Tests call this before constructing a worker service; the service reads
/// the verifier from PostgreSQL on every RPC.
pub(crate) async fn worker_token(
    conn: &mut PgConnection,
    organization: OrganizationId,
    principal: PrincipalId,
    pool: &str,
    verifier: Sha256Digest,
) -> WorkerPoolTokenId {
    let id = WorkerPoolTokenId::generate();
    sqlx::query(
        "INSERT INTO worker_pool_tokens
             (id, organization_id, principal_id, pool, verifier, label)
         VALUES ($1, $2, $3, $4, $5, 'test')
         ON CONFLICT (verifier) DO NOTHING",
    )
    .bind(id.as_uuid())
    .bind(organization.as_uuid())
    .bind(principal.as_uuid())
    .bind(pool)
    .bind(verifier.as_bytes().as_slice())
    .execute(&mut *conn)
    .await
    .expect("worker token is registered");
    // Return the stored row's id, which may belong to an earlier registration
    // of the same verifier.
    let stored: Uuid = sqlx::query_scalar("SELECT id FROM worker_pool_tokens WHERE verifier = $1")
        .bind(verifier.as_bytes().as_slice())
        .fetch_one(&mut *conn)
        .await
        .expect("worker token is readable");
    WorkerPoolTokenId::from_uuid(stored)
}

/// Creates a catalog connection named `name` in `organization`.
pub(crate) async fn connection(
    conn: &mut PgConnection,
    organization: OrganizationId,
    name: &str,
) -> CatalogConnectionId {
    let id = CatalogConnectionId::generate();
    tenancy::create_catalog_connection(
        conn,
        &NewCatalogConnection {
            id,
            organization_id: organization,
            name: name.to_owned(),
            uri: "https://catalog.example/catalog".to_owned(),
            warehouse: Some("warehouse".to_owned()),
            credential_ref: Some("secret/catalog".to_owned()),
        },
    )
    .await
    .expect("connection is created");
    id
}

/// An alias `db.<name>` on `connection`.
pub(crate) fn alias(connection: CatalogConnectionId, name: &str) -> TableAlias {
    TableAlias {
        connection_id: connection,
        namespace: vec!["db".to_owned()],
        name: name.to_owned(),
    }
}

/// Creates a full tenant with a table identity named `slug`.
pub(crate) async fn tenant(conn: &mut PgConnection, slug: &str) -> Tenant {
    let organization = organization(conn, slug).await;
    let user = user(conn, organization, "alice").await;
    let connection = connection(conn, organization, "main").await;
    let table = tables::resolve_table(
        conn,
        organization,
        &alias(connection, "events"),
        Uuid::now_v7(),
    )
    .await
    .expect("table resolves")
    .id;
    // BS-017: a user needs an active grant before admission or publication.
    grants::create_grant(
        conn,
        &NewRoleGrant {
            id: bergsmith_common::ids::RoleGrantId::generate(),
            organization_id: organization,
            principal_id: user,
            role: Role::Administrator,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .expect("administrator grant is created");
    Tenant {
        organization,
        user,
        connection,
        table,
    }
}

/// Returns the constraint a typed write error names.
pub(crate) fn constraint(error: &PersistenceError) -> &str {
    match error {
        PersistenceError::AlreadyExists { constraint, .. }
        | PersistenceError::MissingReference { constraint, .. }
        | PersistenceError::InvalidValue { constraint, .. } => constraint,
        other => panic!("expected a constraint violation, got {other:?}"),
    }
}

/// A 64-character lowercase hex digest made of one repeated digit.
pub(crate) fn digest(digit: char) -> String {
    std::iter::repeat_n(digit, 64).collect()
}

/// Creates a `Planning` job for the tenant's table.
pub(crate) async fn job(conn: &mut PgConnection, tenant: &Tenant) -> JobId {
    let id = JobId::generate();
    jobs::create_job(
        conn,
        &NewJob {
            id,
            organization_id: tenant.organization,
            table_id: tenant.table,
            retry_of: None,
            created_by: tenant.user,
        },
    )
    .await
    .expect("job is created");
    id
}

/// Persists a plan with `task_count` tasks for a `Planning` job.
pub(crate) async fn plan(conn: &mut PgConnection, job: JobId, task_count: usize) -> Vec<TaskId> {
    plan_with_budget(conn, job, task_count, Duration::from_secs(180)).await
}

/// Persists a plan whose job may run for `budget`.
pub(crate) async fn plan_with_budget(
    conn: &mut PgConnection,
    job: JobId,
    task_count: usize,
    budget: Duration,
) -> Vec<TaskId> {
    let task_ids: Vec<TaskId> = (0..task_count).map(|_| TaskId::generate()).collect();
    plans::create_plan(
        conn,
        &NewPlan {
            id: PlanId::generate(),
            job_id: job,
            content: serde_json::json!({ "groups": task_count }),
            content_hash: digest('a'),
            task_ids: task_ids.clone(),
            execution_budget: budget,
        },
    )
    .await
    .expect("plan is created");
    task_ids
}

/// Test-only shortcut that sets a job's state without a transition.
pub(crate) async fn force_job_state(conn: &mut PgConnection, job: JobId, state: JobState) {
    sqlx::query("UPDATE jobs SET state = $2 WHERE id = $1")
        .bind(job.as_uuid())
        .bind(state.as_str())
        .execute(conn)
        .await
        .expect("job state is forced");
}

/// Test-only shortcut that returns a waiting task to `Pending`.
pub(crate) async fn release_task(conn: &mut PgConnection, task: TaskId) {
    sqlx::query("UPDATE tasks SET state = 'Pending', retry_not_before = NULL WHERE id = $1")
        .bind(task.as_uuid())
        .execute(conn)
        .await
        .expect("task is released");
}

/// Test-only shortcut that moves an attempt's lease into the past.
pub(crate) async fn expire_lease(conn: &mut PgConnection, attempt: AttemptId) {
    sqlx::query(
        "UPDATE attempts SET granted_at = now() - interval '2 minutes',
             lease_expires_at = now() - interval '1 minute'
         WHERE id = $1",
    )
    .bind(attempt.as_uuid())
    .execute(conn)
    .await
    .expect("lease is expired");
}

/// Grants `task` to a fresh worker session with a 90 second lease.
pub(crate) async fn grant(
    conn: &mut PgConnection,
    task: TaskId,
) -> Result<Attempt, PersistenceError> {
    let worker_session_id = worker_session(conn, task).await;
    attempts::grant_attempt(
        conn,
        &NewGrant {
            attempt_id: AttemptId::generate(),
            task_id: task,
            worker_session_id,
            lease: Duration::from_secs(90),
        },
    )
    .await
}

/// Creates a service-bound session for a task's tenant.
pub(crate) async fn worker_session(conn: &mut PgConnection, task: TaskId) -> WorkerSessionId {
    let organization: Uuid = sqlx::query_scalar(
        "SELECT organization_id FROM jobs JOIN tasks ON tasks.job_id = jobs.id WHERE tasks.id = $1",
    )
    .bind(task.as_uuid())
    .fetch_one(&mut *conn)
    .await
    .expect("task tenant");
    let principal = PrincipalId::generate();
    tenancy::create_principal(
        conn,
        &NewPrincipal {
            id: principal,
            organization_id: OrganizationId::from_uuid(organization),
            kind: PrincipalKind::Service,
            issuer: "bergsmith:test".into(),
            subject: principal.to_string(),
            display_name: "test worker".into(),
        },
    )
    .await
    .expect("service principal");
    let id = WorkerSessionId::generate();
    bergsmith_persistence::workers::register(
        conn,
        &bergsmith_persistence::workers::Identity {
            organization: OrganizationId::from_uuid(organization),
            principal,
            pool: "test".into(),
        },
        &bergsmith_persistence::workers::Registration {
            id,
            worker_name: "test".into(),
            capabilities: vec!["native-v1".into()],
            memory_bytes: i64::MAX,
            cpu_millis: i32::MAX,
            build_version: "test".into(),
        },
        Duration::from_secs(90),
    )
    .await
    .expect("worker session");
    id
}

/// Takes coordinator leadership under a fresh holder and returns its epoch.
pub(crate) async fn leader(conn: &mut PgConnection) -> i64 {
    bergsmith_persistence::leadership::acquire_leadership(
        conn,
        &Uuid::now_v7().to_string(),
        Duration::from_secs(60),
    )
    .await
    .expect("leadership query succeeds")
    .expect("leadership is free")
    .epoch
}

/// Returns the SQLSTATE of a raw query error.
pub(crate) fn sqlstate(error: &sqlx::Error) -> String {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .expect("a database error with a code")
        .into_owned()
}

/// Creates a tenant named `slug` with a `Running` job whose single task has a
/// granted attempt.
pub(crate) async fn running_attempt(pool: &sqlx::PgPool, slug: &str) -> (JobId, Attempt) {
    let mut conn = pool.acquire().await.expect("connection");
    let tenant = tenant(&mut conn, slug).await;
    let id = job(&mut conn, &tenant).await;
    let task = plan(&mut conn, id, 1).await[0];
    force_job_state(&mut conn, id, JobState::Running).await;
    let attempt = grant(&mut conn, task).await.expect("task is granted");
    (id, attempt)
}

/// Waits for PostgreSQL to confirm a row-lock waiter, without timing sleeps.
pub(crate) async fn wait_until_blocked(conn: &mut PgConnection, waiter: i32, blocker: i32) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let is_waiting: bool = sqlx::query_scalar("SELECT $2 = ANY(pg_blocking_pids($1))")
                .bind(waiter)
                .bind(blocker)
                .fetch_one(&mut *conn)
                .await
                .expect("lock wait is observable");
            if is_waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the operation waits for the held row lock");
}
