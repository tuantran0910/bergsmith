//! The publication-start cancellation boundary, with the PostgreSQL ledger
//! and the Compose Lakekeeper (BS-007.T3).
//!
//! Before send authorization, cancellation prevents the commit. After it,
//! cancellation is too late: the commit may publish, a success is recorded
//! as success, and a non-success ending needs proof of non-publication.

// clippy's `allow-*-in-tests` settings cover only `#[test]` functions; the
// fixtures below fail the test on setup errors.
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures fail the test on setup errors"
)]

#[path = "../../catalog/tests/support/mod.rs"]
mod support;

use std::time::Duration;

use bergsmith_catalog::{
    CatalogConnection, CommitOutcome, PreparedCommit, RejectionKind, SnapshotEvidence, TableName,
};
use bergsmith_common::ids::{CatalogConnectionId, JobId, OrganizationId, PrincipalId, RoleGrantId};
use bergsmith_common::job::JobState;
use bergsmith_common::permission::{GrantScope, Role};
use bergsmith_coordinator::inventory::record_observation;
use bergsmith_coordinator::publication::{PublicationError, send_first_commit};
use bergsmith_persistence::PersistenceError;
use bergsmith_persistence::cancellation::{
    CancelJob, CancellationDisposition, SendAuthorization, authorize_send, request_cancellation,
};
use bergsmith_persistence::grants::{self, NewRoleGrant};
use bergsmith_persistence::jobs::{self, Job, NewJob, Transition};
use bergsmith_persistence::leadership::acquire_leadership;
use bergsmith_persistence::tenancy::{
    self, NewCatalogConnection, NewOrganization, NewPrincipal, PrincipalKind,
};
use sqlx::{PgConnection, PgPool};
use support::Warehouse;
use support::commit::{append, baseline, data_file, rewrite, table_with_files};
use support::proxy::LossyProxy;

/// Creates the job requester and its organization-wide administrator grant.
async fn publisher_principal(db: &mut PgConnection, organization: OrganizationId) -> PrincipalId {
    let created_by = PrincipalId::generate();
    tenancy::create_principal(
        db,
        &NewPrincipal {
            id: created_by,
            organization_id: organization,
            kind: PrincipalKind::User,
            issuer: "https://idp.example".to_owned(),
            subject: format!("publisher-{created_by}"),
            display_name: "Publisher".to_owned(),
        },
    )
    .await
    .unwrap();
    grants::create_grant(
        db,
        &NewRoleGrant {
            id: RoleGrantId::generate(),
            organization_id: organization,
            principal_id: created_by,
            role: Role::Administrator,
            scope: GrantScope::organization(),
            granted_by: None,
        },
    )
    .await
    .unwrap();
    created_by
}

struct Fixture {
    db: sqlx::pool::PoolConnection<sqlx::Postgres>,
    warehouse: Warehouse,
    catalog: CatalogConnection,
    name: TableName,
    job: Job,
    epoch: i64,
    prepared: PreparedCommit,
}

/// A table with one file, a rewrite of it ready to send, and its job in
/// `Committing` under a live leadership lease.
async fn fixture(pool: &PgPool) -> Fixture {
    let (warehouse, catalog, name, files) = table_with_files(&["a"]).await;
    let pinned = baseline(&catalog, &name).await;
    let prepared = rewrite(&pinned, files, vec![data_file(&pinned, "c", 1)])
        .prepare(&pinned)
        .await
        .unwrap();
    let mut db = pool.acquire().await.unwrap();
    let observed = catalog
        .load_table(&name, false)
        .await
        .unwrap()
        .observation();
    let organization = OrganizationId::generate();
    tenancy::create_organization(
        &mut db,
        &NewOrganization {
            id: organization,
            slug: "acme".to_owned(),
            display_name: "Acme".to_owned(),
        },
    )
    .await
    .unwrap();
    let connection = CatalogConnectionId::generate();
    tenancy::create_catalog_connection(
        &mut db,
        &NewCatalogConnection {
            id: connection,
            organization_id: organization,
            name: "lakekeeper".to_owned(),
            uri: support::server().to_string(),
            warehouse: Some(warehouse.name.clone()),
            credential_ref: None,
        },
    )
    .await
    .unwrap();
    let table_id = record_observation(&mut db, organization, connection, &observed)
        .await
        .unwrap();
    // A requester is mandatory; an administrator grant lets the fixture
    // create the intent and authorize the send.
    let created_by = publisher_principal(&mut db, organization).await;
    let mut job = jobs::create_job(
        &mut db,
        &NewJob {
            id: JobId::generate(),
            organization_id: organization,
            table_id,
            retry_of: None,
            created_by,
        },
    )
    .await
    .unwrap();
    for state in [JobState::Queued, JobState::Running, JobState::ReadyToCommit] {
        job = transition(&mut db, &job, state, false).await.unwrap();
    }
    let epoch = acquire_leadership(&mut db, "test", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap()
        .epoch;
    job = bergsmith_persistence::publications::create_intent(
        &mut db,
        &bergsmith_persistence::publications::Intent {
            job_id: job.id,
            publication_id: prepared.expected.publication_id,
            content: serde_json::json!({"namespace": prepared.request.table().namespace, "table_name": prepared.request.table().name}),
            request_body: prepared.request.body().to_vec(),
            request_hash: prepared.request.sha256().to_hex(),
        },
        job.state_revision,
        epoch,
        &[],
    )
    .await
    .unwrap();
    Fixture {
        db,
        warehouse,
        catalog,
        name,
        job,
        epoch,
        prepared,
    }
}

async fn transition(
    db: &mut PgConnection,
    job: &Job,
    to: JobState,
    non_publication_proven: bool,
) -> Result<Job, PersistenceError> {
    jobs::transition_job(
        db,
        &Transition {
            job_id: job.id,
            expected_revision: job.state_revision,
            to,
            reason: None,
            actor: None,
            non_publication_proven,
        },
    )
    .await
}

fn authorization(job: &Job, epoch: i64) -> SendAuthorization {
    SendAuthorization {
        job_id: job.id,
        expected_revision: job.state_revision,
        coordinator_epoch: epoch,
    }
}

impl Fixture {
    async fn cancel(&mut self) -> CancellationDisposition {
        let outcome = request_cancellation(
            &mut self.db,
            &CancelJob {
                job_id: self.job.id,
                requested_by: None,
                reason: Some("operator".to_owned()),
            },
        )
        .await
        .unwrap();
        self.job = outcome.job;
        outcome.disposition
    }

    async fn evidence(&self) -> SnapshotEvidence {
        self.catalog
            .snapshot_evidence(&self.name, &self.prepared.expected)
            .await
            .unwrap()
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_before_send_authorization_prevents_the_commit(pool: PgPool) {
    let mut fixture = fixture(&pool).await;
    let stale = authorization(&fixture.job, fixture.epoch);

    assert_eq!(fixture.cancel().await, CancellationDisposition::Cancelling);
    let refused = send_first_commit(
        &mut fixture.db,
        stale,
        &fixture.catalog,
        &fixture.prepared.request,
        None,
    )
    .await
    .unwrap_err();

    assert!(
        matches!(refused, PublicationError::NotAuthorized(_)),
        "{refused}"
    );
    let current = SendAuthorization {
        expected_revision: fixture.job.state_revision,
        ..stale
    };
    let refused_again = authorize_send(&mut fixture.db, current).await.unwrap_err();
    assert!(
        matches!(refused_again, PersistenceError::JobNotInState { .. }),
        "{refused_again}"
    );
    assert_eq!(fixture.evidence().await, SnapshotEvidence::Absent);
    let cancelled = transition(&mut fixture.db, &fixture.job, JobState::Cancelled, false)
        .await
        .unwrap();
    assert_eq!(cancelled.state, JobState::Cancelled);
    assert!(cancelled.publication_started_at.is_none());
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_after_send_authorization_is_too_late_and_success_is_kept(pool: PgPool) {
    let mut fixture = fixture(&pool).await;
    fixture.job = authorize_send(&mut fixture.db, authorization(&fixture.job, fixture.epoch))
        .await
        .unwrap();

    assert_eq!(
        fixture.cancel().await,
        CancellationDisposition::TooLatePublicationStarted
    );
    assert_eq!(fixture.job.state, JobState::Committing);
    let outcome = fixture
        .catalog
        .commit(&fixture.prepared.request, None)
        .await
        .unwrap();

    assert!(
        matches!(outcome, CommitOutcome::Committed(_)),
        "{outcome:?}"
    );
    fixture.job = transition(&mut fixture.db, &fixture.job, JobState::Succeeded, false)
        .await
        .unwrap();
    assert!(fixture.job.cancellation.is_some());
    assert_eq!(
        fixture.cancel().await,
        CancellationDisposition::TooLateCommitted
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_authorized_send_publishes_and_marks_the_boundary(pool: PgPool) {
    let mut fixture = fixture(&pool).await;

    let sent = send_first_commit(
        &mut fixture.db,
        authorization(&fixture.job, fixture.epoch),
        &fixture.catalog,
        &fixture.prepared.request,
        None,
    )
    .await
    .unwrap();

    assert!(sent.job.publication_started_at.is_some());
    assert_eq!(sent.job.state_revision, fixture.job.state_revision);
    assert!(matches!(sent.outcome, CommitOutcome::Committed(_)));
    let again = send_first_commit(
        &mut fixture.db,
        authorization(&fixture.job, fixture.epoch),
        &fixture.catalog,
        &fixture.prepared.request,
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            again,
            PublicationError::NotAuthorized(PersistenceError::PublicationAlreadyAuthorized { .. })
        ),
        "{again}"
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_rejection_after_authorization_cancels_only_with_proof(pool: PgPool) {
    let mut fixture = fixture(&pool).await;
    append(&fixture.catalog, &fixture.name, &["late"]).await;
    fixture.job = authorize_send(&mut fixture.db, authorization(&fixture.job, fixture.epoch))
        .await
        .unwrap();
    assert_eq!(
        fixture.cancel().await,
        CancellationDisposition::TooLatePublicationStarted
    );

    let outcome = fixture
        .catalog
        .commit(&fixture.prepared.request, None)
        .await
        .unwrap();

    assert!(
        matches!(&outcome, CommitOutcome::Rejected(r) if r.kind == RejectionKind::RequirementFailed),
        "{outcome:?}"
    );
    let unproven = transition(&mut fixture.db, &fixture.job, JobState::Cancelled, false)
        .await
        .unwrap_err();
    assert!(
        matches!(unproven, PersistenceError::JobTransition(_)),
        "{unproven}"
    );
    let cancelled = transition(&mut fixture.db, &fixture.job, JobState::Cancelled, true)
        .await
        .unwrap();
    assert_eq!(cancelled.state, JobState::Cancelled);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_of_an_unknown_outcome_is_recorded_and_evidence_decides(pool: PgPool) {
    let mut fixture = fixture(&pool).await;
    let proxy = LossyProxy::start().await;
    let lossy = fixture.warehouse.connect_via(proxy.url()).await;
    proxy.drop_commit_responses(1);

    let sent = send_first_commit(
        &mut fixture.db,
        authorization(&fixture.job, fixture.epoch),
        &lossy,
        &fixture.prepared.request,
        None,
    )
    .await
    .unwrap();

    assert!(
        matches!(sent.outcome, CommitOutcome::Unknown(_)),
        "{:?}",
        sent.outcome
    );
    fixture.job = transition(
        &mut fixture.db,
        &sent.job,
        JobState::CommitOutcomeUnknown,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        fixture.cancel().await,
        CancellationDisposition::PublicationUnresolved
    );
    assert!(fixture.job.cancellation.is_some());
    assert!(matches!(
        fixture.evidence().await,
        SnapshotEvidence::Published { .. }
    ));
    let succeeded = transition(&mut fixture.db, &fixture.job, JobState::Succeeded, false)
        .await
        .unwrap();
    assert_eq!(succeeded.state, JobState::Succeeded);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_authorized_send_that_cannot_connect_is_reported_unsent(pool: PgPool) {
    let mut fixture = fixture(&pool).await;
    let proxy = LossyProxy::start().await;
    let unreachable = fixture.warehouse.connect_via(proxy.url()).await;
    drop(proxy);

    let error = send_first_commit(
        &mut fixture.db,
        authorization(&fixture.job, fixture.epoch),
        &unreachable,
        &fixture.prepared.request,
        None,
    )
    .await
    .unwrap_err();

    let PublicationError::NotSent { job, .. } = error else {
        panic!("the request is not sent: {error}");
    };
    assert!(job.publication_started_at.is_some());
    assert_eq!(fixture.evidence().await, SnapshotEvidence::Absent);
}
