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

use bergsmith_common::ids::{
    AttemptId, CatalogConnectionId, JobId, OrganizationId, PlanId, PrincipalId, TableId, TaskId,
    WorkerSessionId,
};
use bergsmith_common::job::JobState;
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::attempts::{self, Attempt, NewGrant};
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
            created_by: Some(tenant.user),
        },
    )
    .await
    .expect("job is created");
    id
}

/// Persists a plan with `task_count` tasks for a `Planning` job.
pub(crate) async fn plan(conn: &mut PgConnection, job: JobId, task_count: usize) -> Vec<TaskId> {
    let task_ids: Vec<TaskId> = (0..task_count).map(|_| TaskId::generate()).collect();
    plans::create_plan(
        conn,
        &NewPlan {
            id: PlanId::generate(),
            job_id: job,
            content: serde_json::json!({ "groups": task_count }),
            content_hash: digest('a'),
            task_ids: task_ids.clone(),
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
    attempts::grant_attempt(
        conn,
        &NewGrant {
            attempt_id: AttemptId::generate(),
            task_id: task,
            worker_session_id: WorkerSessionId::generate(),
            lease: Duration::from_secs(90),
        },
    )
    .await
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
