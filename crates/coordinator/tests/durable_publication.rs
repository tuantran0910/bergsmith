//! Native outputs plus PostgreSQL crash points and a controllable REST catalog.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "acceptance fixtures fail on setup errors"
)]

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

/// The execution budget the tests plan with.
const EXECUTION_BUDGET: Duration = Duration::from_secs(180);

use bergsmith_catalog::{Baseline, CatalogConnection, ConnectionConfig, LoadedTable, TableName};
use bergsmith_common::ids::{AttemptId, PlanId};
use bergsmith_common::job::JobState;
use bergsmith_coordinator::operator::{self, OperatorError};
use bergsmith_coordinator::{durable_publication as publication, planning, results};
use bergsmith_fixtures::{Fixture, live_rows, spike};
use bergsmith_maintenance::{
    AttemptContext, ExecutionInput, ExecutionLimits, ExecutionRequest, Executor, NativeExecutor,
    PlanDocument, PlanIds, PlanInput, PlanOutcome, Planner, PlannerConfig,
};
use bergsmith_persistence::{PersistenceError, attempts, cancellation, jobs, publications};
use bergsmith_protocol::artifact::TaskReceipt;
use bergsmith_protocol::hashing::artifact_digest;
use bergsmith_storage::{Location, ObjectStore, StorageProfile};
use iceberg::spec::TableMetadata;
use mockito::{Matcher, Mock, Server, ServerGuard};
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use std::time::Duration;

const TABLE_PATH: &str = "/v1/namespaces/db/tables/t";

struct Run {
    fixture: Fixture,
    store: ObjectStore,
    tenant: support::Tenant,
    job: jobs::Job,
    plan: PlanDocument,
    epoch: i64,
    server: ServerGuard,
    catalog: CatalogConnection,
    baseline: Baseline,
    load: Mock,
    receipts: Vec<TaskReceipt>,
}

async fn load_mock(server: &mut Server, fixture: &Fixture, metadata: &TableMetadata) -> Mock {
    server
        .mock("GET", TABLE_PATH)
        .with_body(
            json!({"metadata-location": fixture.metadata_location(), "metadata": metadata})
                .to_string(),
        )
        .create_async()
        .await
}

async fn setup(conn: &mut PgConnection, zero: bool) -> Run {
    let fixture = if zero {
        spike::fully_deleted_group().unwrap()
    } else {
        spike::small_files_with_duplicates_and_nulls().unwrap()
    }
    .generate()
    .await
    .unwrap();
    let store = ObjectStore::open(&StorageProfile::LocalFs, None).unwrap();
    let mut tenant = support::tenant(conn, "acme").await;
    tenant.table = bergsmith_persistence::tables::resolve_table(
        conn,
        tenant.organization,
        &bergsmith_persistence::tables::TableAlias {
            connection_id: tenant.connection,
            namespace: vec!["db".into()],
            name: "t".into(),
        },
        fixture.metadata().uuid(),
    )
    .await
    .unwrap()
    .id;
    let job_id = support::job(conn, &tenant).await;
    let outcome = Planner::new(PlannerConfig {
        min_input_files: 1,
        min_files_removed: 0,
        ..PlannerConfig::default()
    })
    .unwrap()
    .plan(
        PlanInput {
            metadata: fixture.metadata(),
            metadata_location: Some(fixture.metadata_location()),
            file_io: fixture.file_io(),
            selection: &[],
        },
        PlanIds {
            job_id,
            plan_id: PlanId::generate(),
        },
    )
    .await
    .unwrap();
    let PlanOutcome::Planned(plan) = &outcome else {
        panic!("expected planned rewrite");
    };
    let plan = plan.as_ref().clone();
    let job = jobs::job(conn, tenant.organization, job_id).await.unwrap();
    let queued = planning::record_outcome(conn, &job, &outcome, EXECUTION_BUDGET)
        .await
        .unwrap();
    let job = jobs::transition_job(
        conn,
        &jobs::Transition {
            job_id,
            expected_revision: queued.state_revision,
            to: JobState::Running,
            reason: None,
            actor: None,
            non_publication_proven: false,
        },
    )
    .await
    .unwrap();
    let epoch = support::leader(conn).await;
    let (server, catalog, load) = catalog(&fixture).await;
    let name = TableName {
        namespace: vec!["db".into()],
        name: "t".into(),
    };
    let baseline = Baseline::new(
        &LoadedTable {
            name,
            metadata: fixture.metadata().clone(),
            metadata_location: Some(fixture.metadata_location().into()),
            location: Location::parse(fixture.metadata().location()).unwrap(),
            credentials: vec![],
        },
        fixture.file_io().clone(),
    )
    .unwrap();
    Run {
        fixture,
        store,
        tenant,
        job,
        plan,
        epoch,
        server,
        catalog,
        baseline,
        load,
        receipts: vec![],
    }
}

async fn catalog(fixture: &Fixture) -> (ServerGuard, CatalogConnection, Mock) {
    let mut server = Server::new_async().await;
    let config = server
        .mock("GET", "/v1/config")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .create_async()
        .await;
    let catalog = CatalogConnection::connect(&ConnectionConfig::new(
        url::Url::parse(&server.url()).unwrap(),
    ))
    .await
    .unwrap();
    config.assert_async().await;
    let load = load_mock(&mut server, fixture, fixture.metadata()).await;
    (server, catalog, load)
}

impl Run {
    async fn execute(&mut self, conn: &mut PgConnection, accept: bool) {
        for task in self.plan.tasks() {
            let worker_session_id = support::worker_session(conn, task.task_id()).await;
            let attempt = attempts::grant_attempt(
                conn,
                &attempts::NewGrant {
                    attempt_id: AttemptId::generate(),
                    task_id: task.task_id(),
                    worker_session_id,
                    lease: Duration::from_secs(90),
                },
            )
            .await
            .unwrap();
            let prefix = results::register_attempt_outputs(conn, attempt.id, &self.store)
                .await
                .unwrap();
            let artifact = self.plan.artifact_of(task).unwrap();
            let result = NativeExecutor
                .execute(ExecutionRequest {
                    input: ExecutionInput {
                        artifact: &artifact,
                        artifact_sha256: artifact_digest(&artifact),
                        metadata: self.fixture.metadata(),
                        metadata_location: self.fixture.metadata_location(),
                        file_io: self.fixture.file_io(),
                    },
                    attempt: AttemptContext {
                        attempt_id: attempt.id,
                        generation: attempt.generation,
                    },
                    output_prefix: &prefix,
                    limits: ExecutionLimits::default(),
                })
                .await
                .unwrap();
            let receipt = result.receipt().clone();
            if accept {
                assert_eq!(
                    results::accept_result(conn, &receipt, &self.store, self.fixture.metadata())
                        .await
                        .unwrap()
                        .outcome,
                    attempts::AcceptOutcome::Accepted
                );
            }
            self.receipts.push(receipt);
        }
        self.refresh(conn).await;
    }

    async fn refresh(&mut self, conn: &mut PgConnection) {
        self.job = jobs::job(conn, self.tenant.organization, self.job.id)
            .await
            .unwrap();
    }
    async fn prepare(&mut self, conn: &mut PgConnection) {
        self.job = publication::prepare_publication(
            conn,
            &self.job,
            self.epoch,
            self.tenant.connection,
            &self.catalog,
            &self.baseline,
            &self.store,
        )
        .await
        .unwrap();
    }
    async fn barrier(&self, conn: &mut PgConnection) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM publication_barriers WHERE job_id = $1")
            .bind(self.job.id.as_uuid())
            .fetch_one(conn)
            .await
            .unwrap()
    }
    async fn cancel(&mut self, conn: &mut PgConnection) {
        self.job = cancellation::request_cancellation(
            conn,
            &cancellation::CancelJob {
                job_id: self.job.id,
                requested_by: None,
                reason: None,
            },
        )
        .await
        .unwrap()
        .job;
    }
    async fn authorize(&mut self, conn: &mut PgConnection) {
        self.job = cancellation::authorize_send(
            conn,
            cancellation::SendAuthorization {
                job_id: self.job.id,
                expected_revision: self.job.state_revision,
                coordinator_epoch: self.epoch,
            },
        )
        .await
        .unwrap();
    }
    async fn expose_publication(&mut self, conn: &mut PgConnection) {
        let prepared = publication::load_prepared(conn, self.job.id).await.unwrap();
        let request = prepared.request.decode().unwrap();
        let mut builder = self.fixture.metadata().clone().into_builder(None);
        for requirement in request.requirements {
            requirement.check(Some(self.fixture.metadata())).unwrap();
        }
        for update in request.updates {
            builder = update.apply(builder).unwrap();
        }
        let published = builder.build().unwrap().metadata;
        assert_eq!(
            live_rows(&published, self.fixture.file_io(), None)
                .await
                .unwrap(),
            self.fixture.live_rows().await.unwrap()
        );
        // Simulate another writer advancing main before recovery. The earlier
        // retained snapshot must still prove this exact publication succeeded.
        let head = published.current_snapshot().unwrap();
        let next = iceberg::spec::Snapshot::builder()
            .with_snapshot_id(bergsmith_catalog::new_snapshot_id(&published))
            .with_parent_snapshot_id(Some(head.snapshot_id()))
            .with_sequence_number(published.last_sequence_number() + 1)
            .with_timestamp_ms(head.timestamp_ms() + 1)
            .with_manifest_list(head.manifest_list().to_owned())
            .with_summary(iceberg::spec::Summary {
                operation: iceberg::spec::Operation::Append,
                additional_properties: std::collections::HashMap::new(),
            })
            .with_schema_id(published.current_schema_id())
            .build();
        let published = published
            .into_builder(None)
            .set_branch_snapshot(next, "main")
            .unwrap()
            .build()
            .unwrap()
            .metadata;
        assert!(matches!(
            prepared.expected.find_in(&published),
            bergsmith_catalog::SnapshotEvidence::Published {
                main_head: false,
                ..
            }
        ));
        self.load.remove_async().await;
        self.load = load_mock(&mut self.server, &self.fixture, &published).await;
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn accepted_native_results_create_an_immutable_intent_and_publish_once(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    assert_eq!(run.job.state, JobState::ReadyToCommit);
    run.prepare(&mut conn).await;
    let original = publications::intent(&mut conn, run.job.id).await.unwrap();
    let restored = publication::load_prepared(&mut conn, run.job.id)
        .await
        .unwrap();
    assert_eq!(restored.request.body(), original.request_body);
    let commit = run
        .server
        .mock("POST", TABLE_PATH)
        .match_header("idempotency-key", Matcher::Missing)
        .match_body(original.request_body.clone())
        .with_status(200)
        .with_body("{}")
        .expect(1)
        .create_async()
        .await;
    run.job = publication::publish(&mut conn, &run.job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, JobState::Succeeded);
    assert_eq!(run.barrier(&mut conn).await, 0);
    assert_eq!(
        publications::output_inventory(&mut conn, run.job.id)
            .await
            .unwrap()[0]
            .disposition,
        publications::OutputDisposition::Published
    );
    assert!(
        publication::publish(&mut conn, &run.job, run.epoch, &run.catalog)
            .await
            .is_err()
    );
    assert_eq!(
        publications::intent(&mut conn, run.job.id).await.unwrap(),
        original
    );
    commit.assert_async().await;
    let mutation =
        sqlx::query("UPDATE publication_intents SET request_body = request_body WHERE job_id = $1")
            .bind(run.job.id.as_uuid())
            .execute(&mut *conn)
            .await
            .unwrap_err();
    assert_eq!(support::sqlstate(&mutation), "23001");
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn crash_after_intent_before_authorization_can_still_cancel(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    run.job = publication::reconcile(&mut conn, &run.job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, JobState::Committing);
    assert!(run.job.publication_started_at.is_none());
    run.cancel(&mut conn).await;
    assert!(
        publication::publish(&mut conn, &run.job, run.epoch, &run.catalog)
            .await
            .is_err()
    );
    let pending = publications::recoverable_jobs(&mut conn).await.unwrap();
    assert_eq!(pending[0].state, JobState::Cancelling);
    run.job = publication::reconcile(&mut conn, &pending[0], run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, JobState::Cancelled);
    assert_eq!(run.barrier(&mut conn).await, 0);
    let replay = results::accept_result(
        &mut conn,
        &run.receipts[0],
        &run.store,
        run.fixture.metadata(),
    )
    .await
    .unwrap();
    assert_eq!(replay.outcome, attempts::AcceptOutcome::AlreadyAccepted);
    assert_eq!(replay.job_state, JobState::Cancelled);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn crash_after_authorization_retains_barrier_until_matching_evidence(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    run.authorize(&mut conn).await;
    let restored = publications::recoverable_jobs(&mut conn).await.unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].id, run.job.id);
    run.job = publication::reconcile(&mut conn, &run.job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, JobState::CommitOutcomeUnknown);
    assert_eq!(run.barrier(&mut conn).await, 1);
    run.cancel(&mut conn).await;
    assert_eq!(
        publications::output_inventory(&mut conn, run.job.id)
            .await
            .unwrap()[0]
            .disposition,
        publications::OutputDisposition::PublicationUnresolved
    );
    publications::pend_disposition(
        &mut conn,
        run.job.id,
        run.epoch,
        JobState::Failed,
        "ExecutionFailed",
    )
    .await
    .unwrap();
    assert!(
        publications::resolve(
            &mut conn,
            run.job.id,
            run.job.state_revision,
            run.epoch,
            JobState::Failed,
            "Unproven",
            false
        )
        .await
        .is_err()
    );
    run.expose_publication(&mut conn).await;
    run.job = publication::reconcile(&mut conn, &run.job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, JobState::Succeeded);
    assert!(run.job.cancellation.is_some());
    assert_eq!(run.barrier(&mut conn).await, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn unknown_response_is_not_replayed_and_missing_history_does_not_fail(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    let commit = run
        .server
        .mock("POST", TABLE_PATH)
        .with_status(503)
        .expect(1)
        .create_async()
        .await;
    run.job = publication::publish(&mut conn, &run.job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, JobState::CommitOutcomeUnknown);
    for _ in 0..2 {
        run.job = publication::reconcile(&mut conn, &run.job, run.epoch, &run.catalog)
            .await
            .unwrap();
    }
    assert!(
        publication::publish(&mut conn, &run.job, run.epoch, &run.catalog)
            .await
            .is_err()
    );
    assert_eq!(run.barrier(&mut conn).await, 1);
    commit.assert_async().await;
    run.load.remove_async().await;
    let unavailable = run
        .server
        .mock("GET", TABLE_PATH)
        .with_status(503)
        .create_async()
        .await;
    assert!(
        publication::reconcile(&mut conn, &run.job, run.epoch, &run.catalog)
            .await
            .is_err()
    );
    assert_eq!(run.barrier(&mut conn).await, 1);
    unavailable.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn first_request_rejections_are_conclusive_and_release_the_barrier(pool: PgPool) {
    rejected_first_send(&pool, 409, JobState::ReplanRequired).await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn malformed_first_request_fails_without_publication(pool: PgPool) {
    rejected_first_send(&pool, 400, JobState::Failed).await;
}

async fn rejected_first_send(pool: &PgPool, status: usize, state: JobState) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    let commit = run
        .server
        .mock("POST", TABLE_PATH)
        .with_status(status)
        .create_async()
        .await;
    run.job = publication::publish(&mut conn, &run.job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, state);
    assert_eq!(run.barrier(&mut conn).await, 0);
    commit.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn zero_output_receipt_still_publishes_source_removal(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, true).await;
    run.execute(&mut conn, true).await;
    assert!(run.receipts.iter().all(|r| r.outputs.is_empty()));
    run.prepare(&mut conn).await;
    run.authorize(&mut conn).await;
    run.expose_publication(&mut conn).await;
    run.job = publication::reconcile(&mut conn, &run.job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, JobState::Succeeded);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn invalid_worker_receipts_never_become_durable_or_ready(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, false).await;
    let original = run.receipts[0].clone();
    for mutation in 0..12 {
        let mut receipt = original.clone();
        match mutation {
            0 => receipt.artifact_sha256[0] ^= 1,
            1 => receipt.generation += 1,
            2 => receipt.outputs[0].file_size_bytes += 1,
            3 => receipt.outputs[0].record_count += 1,
            4 => receipt.outputs.push(receipt.outputs[0].clone()),
            5 => receipt.outputs[0].partition.as_mut().unwrap().spec_id += 1,
            6 => receipt.outputs[0].columns[0].value_count += 1,
            7 => receipt.metrics.as_mut().unwrap().rows_deleted += 1,
            8 => {
                receipt.outputs[0].path = run
                    .fixture
                    .path_of("a")
                    .unwrap_or(run.fixture.metadata_location())
                    .into();
            }
            9 => receipt.outputs[0].columns[0].lower_bound = Some(999_i64.to_le_bytes().to_vec()),
            10 => receipt.outputs[0].split_offsets.push(999),
            _ => receipt.outputs[0].columns[0].null_value_count = -1,
        }
        assert!(
            results::accept_result(&mut conn, &receipt, &run.store, run.fixture.metadata())
                .await
                .is_err(),
            "mutation {mutation}"
        );
        assert!(
            publications::accepted_receipts(&mut conn, run.job.id)
                .await
                .unwrap()
                .is_empty()
        );
    }
    run.cancel(&mut conn).await;
    assert_eq!(
        results::accept_result(&mut conn, &original, &run.store, run.fixture.metadata())
            .await
            .unwrap()
            .outcome,
        attempts::AcceptOutcome::Cancelled
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn expiry_after_output_close_fences_acceptance_and_retains_the_prefix(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, false).await;
    let receipt = &run.receipts[0];
    let id = receipt.attempt_id.parse().unwrap();
    support::expire_lease(&mut conn, id).await;
    assert_eq!(
        results::accept_result(&mut conn, receipt, &run.store, run.fixture.metadata())
            .await
            .unwrap()
            .outcome,
        attempts::AcceptOutcome::StaleAttempt
    );
    assert!(
        publications::accepted_receipts(&mut conn, run.job.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        publications::output_inventory(&mut conn, run.job.id)
            .await
            .unwrap()
            .len(),
        run.plan.tasks().len()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn failed_inventory_write_rolls_back_receipt_acceptance_and_ready_transition(pool: PgPool) {
    use prost::Message;
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, false).await;
    let receipt = &run.receipts[0];
    let submission = attempts::ReceiptSubmission {
        attempt_id: receipt.attempt_id.parse().unwrap(),
        generation: receipt.generation,
        content: json!(receipt.encode_to_vec()),
        content_hash: bergsmith_protocol::hashing::receipt_digest(receipt).to_hex(),
    };
    let prefix = Location::parse(run.plan.baseline().data_location())
        .unwrap()
        .attempt_output_prefix(run.job.id, submission.attempt_id);
    let object = (
        receipt.outputs[0].path.clone(),
        receipt.outputs[0].file_size_bytes,
    );
    assert!(
        publications::accept_outputs(
            &mut conn,
            &submission,
            &prefix.to_string(),
            &[object.clone(), object]
        )
        .await
        .is_err()
    );
    assert!(
        publications::accepted_receipts(&mut conn, run.job.id)
            .await
            .unwrap()
            .is_empty()
    );
    run.refresh(&mut conn).await;
    assert_eq!(run.job.state, JobState::Running);
    assert!(
        publications::output_inventory(&mut conn, run.job.id)
            .await
            .unwrap()[0]
            .objects
            .is_empty()
    );
    assert_eq!(
        results::accept_result(
            &mut conn,
            &run.receipts[0],
            &run.store,
            run.fixture.metadata()
        )
        .await
        .unwrap()
        .outcome,
        attempts::AcceptOutcome::Accepted
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn unexpected_finalized_objects_block_receipt_acceptance(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, false).await;
    let receipt = &run.receipts[0];
    let prefix = Location::parse(run.plan.baseline().data_location())
        .unwrap()
        .attempt_output_prefix(run.job.id, receipt.attempt_id.parse().unwrap());
    run.store
        .write(
            &prefix.child("unexpected.parquet").unwrap(),
            b"incomplete output".as_slice().into(),
        )
        .await
        .unwrap();
    assert!(
        results::accept_result(&mut conn, receipt, &run.store, run.fixture.metadata())
            .await
            .is_err()
    );
    assert!(
        results::register_attempt_outputs(
            &mut conn,
            receipt.attempt_id.parse().unwrap(),
            &run.store
        )
        .await
        .is_err()
    );
    assert!(
        publications::accepted_receipts(&mut conn, run.job.id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn cancellation_during_proposal_preparation_prevents_intent_creation(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    let before = run.job.clone();
    run.cancel(&mut conn).await;
    assert!(
        publication::prepare_publication(
            &mut conn,
            &before,
            run.epoch,
            run.tenant.connection,
            &run.catalog,
            &run.baseline,
            &run.store
        )
        .await
        .is_err()
    );
    assert!(publications::intent(&mut conn, run.job.id).await.is_err());
    assert_eq!(run.barrier(&mut conn).await, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_second_job_on_the_same_canonical_table_cannot_create_an_intent(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    let other_id = support::job(&mut conn, &run.tenant).await;
    support::force_job_state(&mut conn, other_id, JobState::ReadyToCommit).await;
    let mut other = publications::intent(&mut conn, run.job.id).await.unwrap();
    other.job_id = other_id;
    other.publication_id = bergsmith_common::ids::PublicationId::generate();
    let error = publications::create_intent(&mut conn, &other, 0, run.epoch, &[])
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PersistenceError::AlreadyExists {
            entity: "table publication barrier",
            ..
        }
    ));
    assert!(publications::intent(&mut conn, other_id).await.is_err());
    assert_eq!(
        jobs::job(&mut conn, run.tenant.organization, other_id)
            .await
            .unwrap()
            .state,
        JobState::ReadyToCommit
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn baseline_drift_after_intent_prevents_send_authorization(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    assert_eq!(
        publication::publication_connection(&mut conn, run.job.id)
            .await
            .unwrap(),
        run.tenant.connection
    );
    run.load.remove_async().await;
    let changed = Location::parse(run.fixture.metadata().location())
        .unwrap()
        .child("metadata")
        .unwrap()
        .child("changed.metadata.json")
        .unwrap();
    let load = run
        .server
        .mock("GET", TABLE_PATH)
        .with_body(
            json!({"metadata-location": changed.to_string(), "metadata": run.fixture.metadata()})
                .to_string(),
        )
        .create_async()
        .await;
    let commit = run
        .server
        .mock("POST", TABLE_PATH)
        .expect(0)
        .create_async()
        .await;
    run.job = publication::publish(&mut conn, &run.job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, JobState::ReplanRequired);
    assert!(run.job.publication_started_at.is_none());
    assert_eq!(run.barrier(&mut conn).await, 0);
    load.assert_async().await;
    commit.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn mismatched_retained_snapshot_never_resolves_publication(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    run.authorize(&mut conn).await;
    run.expose_publication(&mut conn).await;
    let prepared = publication::load_prepared(&mut conn, run.job.id)
        .await
        .unwrap();
    let loaded = run
        .catalog
        .load_table(prepared.request.table(), false)
        .await
        .unwrap();
    let mut metadata = serde_json::to_value(loaded.metadata).unwrap();
    let snapshots = metadata["snapshots"].as_array_mut().unwrap();
    let snapshot = snapshots
        .iter_mut()
        .find(|s| s["snapshot-id"].as_i64() == Some(prepared.expected.snapshot_id))
        .unwrap();
    snapshot["summary"]["bergsmith.proposal-sha256"] = json!("different proposal");
    run.load.remove_async().await;
    run.load = run
        .server
        .mock("GET", TABLE_PATH)
        .with_body(
            json!({"metadata-location": run.fixture.metadata_location(), "metadata": metadata})
                .to_string(),
        )
        .create_async()
        .await;
    run.job = publication::reconcile(&mut conn, &run.job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, JobState::CommitOutcomeUnknown);
    assert_eq!(
        run.job.state_reason.as_deref(),
        Some("SnapshotEvidenceMismatch")
    );
    assert_eq!(run.barrier(&mut conn).await, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn scheduler_startup_recovers_native_publication_evidence_without_resending(pool: PgPool) {
    use bergsmith_coordinator::scheduling::{Scheduler, SchedulerConfig};
    use bergsmith_coordinator::worker_access::{CatalogWorkerAccess, WorkerTableBinding};
    use bergsmith_coordinator::workers::{WorkerConfig, WorkerService};
    use std::sync::Arc;
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    run.authorize(&mut conn).await;
    assert_eq!(run.barrier(&mut conn).await, 1);
    run.expose_publication(&mut conn).await;
    let never_send = run
        .server
        .mock("POST", TABLE_PATH)
        .expect(0)
        .create_async()
        .await;
    let job_id = run.job.id;
    let provider = Arc::new(CatalogWorkerAccess::new(std::collections::BTreeMap::from(
        [(
            job_id,
            WorkerTableBinding {
                catalog: Arc::new(run.catalog),
                table: TableName {
                    namespace: vec!["db".into()],
                    name: "t".into(),
                },
                profile: StorageProfile::LocalFs,
            },
        )],
    )));
    let worker = WorkerService::new(
        pool.clone(),
        run.epoch,
        WorkerConfig::default(),
        provider.clone(),
    )
    .unwrap();
    let scheduler = Scheduler::new(worker, provider, SchedulerConfig::default()).unwrap();
    drop(conn);
    scheduler.reconcile().await.unwrap();
    let recovered = jobs::job(
        &mut pool.acquire().await.unwrap(),
        run.tenant.organization,
        job_id,
    )
    .await
    .unwrap();
    assert_eq!(recovered.state, JobState::Succeeded);
    let barriers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM publication_barriers WHERE job_id=$1")
            .bind(job_id.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(barriers, 0);
    assert!(scheduler.worker_service().ready_tasks().is_empty());
    never_send.assert_async().await;
}

/// A second connection to the run's mock catalog, optionally with OAuth2.
async fn reconnect(run: &mut Run, auth: bergsmith_catalog::Auth) -> CatalogConnection {
    let config = run
        .server
        .mock("GET", "/v1/config")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .create_async()
        .await;
    let mut settings = ConnectionConfig::new(url::Url::parse(&run.server.url()).unwrap());
    settings.auth = auth;
    let catalog = CatalogConnection::connect(&settings).await.unwrap();
    config.assert_async().await;
    catalog
}

fn recovery_scheduler(
    pool: &PgPool,
    run: &Run,
    catalog: CatalogConnection,
    config: bergsmith_coordinator::scheduling::SchedulerConfig,
) -> bergsmith_coordinator::scheduling::Scheduler {
    use bergsmith_coordinator::scheduling::Scheduler;
    use bergsmith_coordinator::worker_access::{CatalogWorkerAccess, WorkerTableBinding};
    use bergsmith_coordinator::workers::{WorkerConfig, WorkerService};
    use std::sync::Arc;
    let provider = Arc::new(CatalogWorkerAccess::new(std::collections::BTreeMap::from(
        [(
            run.job.id,
            WorkerTableBinding {
                catalog: Arc::new(catalog),
                table: TableName {
                    namespace: vec!["db".into()],
                    name: "t".into(),
                },
                profile: StorageProfile::LocalFs,
            },
        )],
    )));
    let worker = WorkerService::new(
        pool.clone(),
        run.epoch,
        WorkerConfig::default(),
        provider.clone(),
    )
    .unwrap();
    Scheduler::new(worker, provider, config).unwrap()
}

async fn current(pool: &PgPool, run: &Run) -> jobs::Job {
    jobs::job(
        &mut pool.acquire().await.unwrap(),
        run.tenant.organization,
        run.job.id,
    )
    .await
    .unwrap()
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn periodic_recovery_leaves_an_in_flight_send_to_its_live_publisher(pool: PgPool) {
    use bergsmith_coordinator::scheduling::SchedulerConfig;
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    let catalog = reconnect(&mut run, bergsmith_catalog::Auth::None).await;
    let scheduler = recovery_scheduler(&pool, &run, catalog, SchedulerConfig::default());
    // Startup pass: the intent is not yet authorized, so nothing changes.
    scheduler.reconcile().await.unwrap();
    assert_eq!(current(&pool, &run).await.state, JobState::Committing);
    // The live publisher authorizes and is awaiting the catalog's response.
    run.authorize(&mut conn).await;
    scheduler.reconcile().await.unwrap();
    let in_flight = current(&pool, &run).await;
    assert_eq!(in_flight.state, JobState::Committing);
    // The publisher's conclusive rejection is then recorded, not lost.
    let resolved = publications::resolve(
        &mut conn,
        in_flight.id,
        in_flight.state_revision,
        run.epoch,
        JobState::ReplanRequired,
        "CommitRejected",
        true,
    )
    .await
    .unwrap();
    assert_eq!(resolved.state, JobState::ReplanRequired);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn startup_recovery_marks_an_authorized_send_unknown_and_a_lost_rejection_proves_nothing(
    pool: PgPool,
) {
    use bergsmith_coordinator::scheduling::SchedulerConfig;
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    run.authorize(&mut conn).await;
    // The request is rejected conclusively, but the coordinator crashes
    // before recording the response.
    let prepared = publication::load_prepared(&mut conn, run.job.id)
        .await
        .unwrap();
    let rejected = run
        .server
        .mock("POST", TABLE_PATH)
        .with_status(409)
        .expect(1)
        .create_async()
        .await;
    assert!(matches!(
        run.catalog.commit(&prepared.request, None).await.unwrap(),
        bergsmith_catalog::CommitOutcome::Rejected(_)
    ));
    rejected.assert_async().await;
    let catalog = reconnect(&mut run, bergsmith_catalog::Auth::None).await;
    let scheduler = recovery_scheduler(&pool, &run, catalog, SchedulerConfig::default());
    scheduler.reconcile().await.unwrap();
    let unknown = current(&pool, &run).await;
    assert_eq!(unknown.state, JobState::CommitOutcomeUnknown);
    assert_eq!(scheduler.unresolved_publications(), 1);
    assert_eq!(run.barrier(&mut conn).await, 1);
    // Nothing automatic resolves it: later passes keep the barrier.
    scheduler.reconcile().await.unwrap();
    assert_eq!(
        current(&pool, &run).await.state,
        JobState::CommitOutcomeUnknown
    );
    // An operator who has established non-publication resolves it, audited.
    let operator = run.tenant.user;
    let failed = publication::attest_non_publication(
        &mut conn,
        &unknown,
        run.epoch,
        &run.catalog,
        operator,
        "INC-0",
    )
    .await
    .unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(
        failed.state_reason.as_deref(),
        Some(publications::OPERATOR_ATTESTED_NON_PUBLICATION)
    );
    assert_eq!(run.barrier(&mut conn).await, 0);
    let events = jobs::job_events(&mut conn, run.job.id).await.unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.actor, Some(operator));
    assert_eq!(
        last.reason.as_deref(),
        Some(publications::OPERATOR_ATTESTED_NON_PUBLICATION)
    );
    scheduler.reconcile().await.unwrap();
    assert_eq!(scheduler.unresolved_publications(), 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn operator_attestation_never_overrides_matching_publication_evidence(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    run.authorize(&mut conn).await;
    run.job = publication::reconcile(&mut conn, &run.job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, JobState::CommitOutcomeUnknown);
    let operator = run.tenant.user;
    // A stale inspected revision is refused.
    let stale = jobs::Job {
        state_revision: run.job.state_revision - 1,
        ..run.job.clone()
    };
    assert!(
        publication::attest_non_publication(
            &mut conn,
            &stale,
            run.epoch,
            &run.catalog,
            operator,
            "INC-0"
        )
        .await
        .is_err()
    );
    run.expose_publication(&mut conn).await;
    let job = publication::attest_non_publication(
        &mut conn,
        &run.job,
        run.epoch,
        &run.catalog,
        operator,
        "INC-0",
    )
    .await
    .unwrap();
    assert_eq!(job.state, JobState::Succeeded);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_fenced_epoch_cannot_record_publication_outcomes(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    run.authorize(&mut conn).await;
    run.job = publication::reconcile(&mut conn, &run.job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, JobState::CommitOutcomeUnknown);
    let holder: String = sqlx::query_scalar("SELECT holder FROM coordinator_leadership")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    bergsmith_persistence::leadership::release_leadership(&mut conn, &holder, run.epoch)
        .await
        .unwrap();
    let fenced = run.epoch;
    let next = support::leader(&mut conn).await;
    assert_ne!(next, fenced);
    let lost = |error: &PersistenceError| matches!(error, PersistenceError::LeadershipLost { .. });
    let error = publications::pend_disposition(
        &mut conn,
        run.job.id,
        fenced,
        JobState::Failed,
        "ExecutionFailed",
    )
    .await
    .unwrap_err();
    assert!(lost(&error), "{error}");
    let error = publications::resolve(
        &mut conn,
        run.job.id,
        run.job.state_revision,
        fenced,
        JobState::Failed,
        "CommitRejected",
        true,
    )
    .await
    .unwrap_err();
    assert!(lost(&error), "{error}");
    let operator = run.tenant.user;
    let error = publications::attest_non_publication(
        &mut conn,
        &publications::Attestation {
            job_id: run.job.id,
            expected_revision: run.job.state_revision,
            epoch: fenced,
            actor: operator,
            evidence: "INC-0",
            snapshot_matched: false,
        },
    )
    .await
    .unwrap_err();
    assert!(lost(&error), "{error}");
    run.expose_publication(&mut conn).await;
    assert!(
        publication::reconcile(&mut conn, &run.job, fenced, &run.catalog)
            .await
            .is_err()
    );
    assert_eq!(
        current(&pool, &run).await.state,
        JobState::CommitOutcomeUnknown
    );
    assert_eq!(run.barrier(&mut conn).await, 1);
    publications::pend_disposition(
        &mut conn,
        run.job.id,
        next,
        JobState::Failed,
        "ExecutionFailed",
    )
    .await
    .unwrap();
    let job = publication::reconcile(&mut conn, &run.job, next, &run.catalog)
        .await
        .unwrap();
    assert_eq!(job.state, JobState::Succeeded);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_connection_failure_after_authorization_fails_and_releases_the_barrier(pool: PgPool) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let table = json!({
        "metadata-location": run.fixture.metadata_location(),
        "metadata": run.fixture.metadata(),
    })
    .to_string();
    let server = tokio::spawn(async move {
        let mut listener = Some(listener);
        for (path, body) in [
            ("/v1/config", r#"{"defaults":{},"overrides":{}}"#.to_owned()),
            (TABLE_PATH, table),
        ] {
            let (mut stream, _) = listener.as_ref().unwrap().accept().await.unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                assert!(headers.len() < 16 * 1024);
                headers.push(stream.read_u8().await.unwrap());
            }
            assert!(
                String::from_utf8(headers)
                    .unwrap()
                    .starts_with(&format!("GET {path}"))
            );
            if path == TABLE_PATH {
                // Stop accepting connections before the baseline reply is
                // delivered. The complete reply still reaches the client;
                // its subsequent authorized POST cannot establish a connection.
                drop(listener.take());
            }
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    let catalog = CatalogConnection::connect(&ConnectionConfig::new(
        url::Url::parse(&format!("http://{address}")).unwrap(),
    ))
    .await
    .unwrap();
    let failed = publication::publish(&mut conn, &run.job, run.epoch, &catalog)
        .await
        .unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.state_reason.as_deref(), Some("CommitNotSent"));
    assert!(failed.publication_started_at.is_some());
    assert_eq!(run.barrier(&mut conn).await, 0);
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_token_failure_before_authorization_leaves_the_intent_publishable(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    // Zero-lifetime tokens are fetched for every request: connect, the
    // baseline reload, then the commit, whose token fetch fails.
    let token_url = url::Url::parse(&format!("{}/oauth/token", run.server.url())).unwrap();
    let issued = run
        .server
        .mock("POST", "/oauth/token")
        .with_body(r#"{"access_token":"t","token_type":"bearer","expires_in":0}"#)
        .expect(2)
        .create_async()
        .await;
    let refused = run
        .server
        .mock("POST", "/oauth/token")
        .with_status(503)
        .expect(1)
        .create_async()
        .await;
    let catalog = reconnect(
        &mut run,
        bergsmith_catalog::Auth::OAuth2 {
            token_url,
            client_id: "bergsmith".into(),
            client_secret: bergsmith_common::secret::Secret::new("s3cret"),
            scope: None,
        },
    )
    .await;
    let never = run
        .server
        .mock("POST", TABLE_PATH)
        .expect(0)
        .create_async()
        .await;
    let error = publication::publish(&mut conn, &run.job, run.epoch, &catalog)
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            publication::DurablePublicationError::Catalog(
                bergsmith_catalog::CatalogError::Authentication { status: 503 }
            )
        ),
        "{error}"
    );
    issued.assert_async().await;
    refused.assert_async().await;
    never.assert_async().await;
    let unchanged = current(&pool, &run).await;
    assert_eq!(unchanged.state, JobState::Committing);
    assert!(unchanged.publication_started_at.is_none());
    // A restart recovery leaves the unauthorized intent publishable, and the
    // same intent then publishes once.
    run.job = publication::reconcile(&mut conn, &unchanged, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert!(run.job.publication_started_at.is_none());
    never.remove_async().await;
    let commit = run
        .server
        .mock("POST", TABLE_PATH)
        .with_status(200)
        .with_body("{}")
        .expect(1)
        .create_async()
        .await;
    run.job = publication::publish(&mut conn, &run.job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, JobState::Succeeded);
    commit.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unsent_stop_cannot_override_authorization_at_the_same_revision(pool: PgPool) {
    use sqlx::Connection as _;

    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    let revision = run.job.state_revision;
    // An unlocked read still sees an unsent job while authorization holds
    // the job lock. The stop must check again after that lock is released.
    let mut authorization = conn.begin().await.unwrap();
    run.authorize(&mut authorization).await;
    assert_eq!(run.job.state_revision, revision);

    let mut stopper = pool.acquire().await.unwrap();
    let name = format!("unsent-stop-{}", uuid::Uuid::now_v7());
    sqlx::query("SELECT set_config('application_name', $1, false)")
        .bind(&name)
        .execute(&mut *stopper)
        .await
        .unwrap();
    let job_id = run.job.id;
    let epoch = run.epoch;
    let stopping = tokio::spawn(async move {
        publications::resolve_unsent(
            &mut stopper,
            job_id,
            revision,
            epoch,
            JobState::Failed,
            "PublicationFailed",
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        loop {
            tick.tick().await;
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity
                 WHERE application_name=$1 AND wait_event_type='Lock')",
            )
            .bind(&name)
            .fetch_one(&pool)
            .await
            .unwrap();
            if blocked {
                break;
            }
        }
    })
    .await
    .unwrap();
    authorization.commit().await.unwrap();
    assert!(stopping.await.unwrap().unwrap().is_none());

    let stopped = publications::resolve_unsent(
        &mut conn,
        job_id,
        revision,
        epoch,
        JobState::ReplanRequired,
        "PublicationBaselineChanged",
    )
    .await
    .unwrap();
    assert!(
        stopped.is_none(),
        "an authorized send must retain its barrier"
    );
    run.refresh(&mut conn).await;
    assert_eq!(run.job.state, JobState::Committing);
    assert!(run.job.publication_started_at.is_some());
    assert_eq!(run.barrier(&mut conn).await, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn an_unsent_stop_is_fenced_by_the_coordinator_epoch(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    let holder: String = sqlx::query_scalar("SELECT holder FROM coordinator_leadership")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    bergsmith_persistence::leadership::release_leadership(&mut conn, &holder, run.epoch)
        .await
        .unwrap();
    let next = support::leader(&mut conn).await;
    let error = publications::resolve_unsent(
        &mut conn,
        run.job.id,
        run.job.state_revision,
        run.epoch,
        JobState::Failed,
        "PublicationFailed",
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PersistenceError::LeadershipLost { .. }));
    assert_eq!(run.barrier(&mut conn).await, 1);

    let resolved = publications::resolve_unsent(
        &mut conn,
        run.job.id,
        run.job.state_revision,
        next,
        JobState::ReplanRequired,
        "PublicationBaselineChanged",
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resolved.state, JobState::ReplanRequired);
    assert!(resolved.publication_started_at.is_none());
    assert_eq!(run.barrier(&mut conn).await, 0);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_runtime_retries_a_401_before_authorizing_an_intent(pool: PgPool) {
    use bergsmith_coordinator::runtime::config::RuntimeProfiles;
    use bergsmith_coordinator::runtime::{PublicationService, RuntimeCatalogs};
    use bergsmith_persistence::scheduling::RetryPolicy;
    use bergsmith_persistence::{Database, DatabaseConfig, DatabaseUrl};
    use sqlx::ConnectOptions as _;
    use std::sync::Arc;

    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    sqlx::query(
        "UPDATE catalog_connections SET uri=$2, warehouse=NULL, credential_ref=NULL WHERE id=$1",
    )
    .bind(run.tenant.connection.as_uuid())
    .bind(run.server.url())
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);

    let mut config = DatabaseConfig::new(DatabaseUrl::new(
        pool.connect_options().to_url_lossy().to_string(),
    ));
    config.max_connections = 1;
    let database = Database::connect(&config).await.unwrap();
    let catalogs = Arc::new(RuntimeCatalogs::new(
        database,
        RuntimeProfiles {
            default: bergsmith_storage::StorageProfiles::default(),
            connections: std::collections::BTreeMap::new(),
        },
    ));
    let service = PublicationService::new(
        pool.clone(),
        catalogs,
        run.epoch,
        publication::PublicationConfig::default(),
        RetryPolicy::default(),
        false,
        bergsmith_coordinator::runtime::config::OrphanConfig::default(),
    );
    run.load.remove_async().await;
    let rejected = run
        .server
        .mock("GET", TABLE_PATH)
        .with_status(401)
        .expect(1)
        .create_async()
        .await;
    let commit = run
        .server
        .mock("POST", TABLE_PATH)
        .with_status(200)
        .with_body("{}")
        .expect(1)
        .create_async()
        .await;
    let summary = service.pass().await.unwrap();
    assert_eq!(summary.deferred, 1);
    assert_eq!(summary.failed, 0);
    rejected.assert_async().await;
    let mut conn = pool.acquire().await.unwrap();
    run.refresh(&mut conn).await;
    assert_eq!(run.job.state, JobState::Committing);
    assert!(run.job.publication_started_at.is_none());
    assert_eq!(run.barrier(&mut conn).await, 1);
    drop(conn);

    rejected.remove_async().await;
    run.load = load_mock(&mut run.server, &run.fixture, run.fixture.metadata()).await;
    // Advance only the retry clock, then restore real time before network I/O.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::time::resume();
    let summary = service.pass().await.unwrap();
    assert_eq!(summary.published, 1);
    let mut conn = pool.acquire().await.unwrap();
    run.refresh(&mut conn).await;
    assert_eq!(run.job.state, JobState::Succeeded);
    assert!(run.job.publication_started_at.is_some());
    assert_eq!(run.barrier(&mut conn).await, 0);
    commit.assert_async().await;
}

/// Points the tenant's stored connection at the mock catalog, so the operator
/// command reopens it the way a deployment would.
async fn expose_connection(conn: &mut PgConnection, run: &mut Run) {
    sqlx::query(
        "UPDATE catalog_connections SET uri = $1, warehouse = NULL, credential_ref = NULL
         WHERE id = $2",
    )
    .bind(run.server.url())
    .bind(run.tenant.connection.as_uuid())
    .execute(&mut *conn)
    .await
    .unwrap();
    run.server
        .mock("GET", "/v1/config")
        .with_body(r#"{"defaults":{},"overrides":{}}"#)
        .create_async()
        .await;
}

async fn unknown_outcome(conn: &mut PgConnection) -> Run {
    let mut run = setup(conn, false).await;
    run.execute(conn, true).await;
    run.prepare(conn).await;
    run.authorize(conn).await;
    run.job = publication::reconcile(conn, &run.job, run.epoch, &run.catalog)
        .await
        .unwrap();
    assert_eq!(run.job.state, JobState::CommitOutcomeUnknown);
    expose_connection(conn, &mut run).await;
    run
}

async fn disabled_runtime_recovery(pool: PgPool, mark_unknown: bool) {
    use bergsmith_coordinator::runtime::catalogs::{RuntimeCatalogs, RuntimeWorkerAccess};
    use bergsmith_coordinator::runtime::config::RuntimeProfiles;
    use bergsmith_coordinator::scheduling::SchedulingAccess as _;
    use bergsmith_persistence::Database;
    use std::sync::Arc;

    let mut conn = pool.acquire().await.unwrap();
    let mut run = setup(&mut conn, false).await;
    run.execute(&mut conn, true).await;
    run.prepare(&mut conn).await;
    run.authorize(&mut conn).await;
    if mark_unknown {
        run.job = publication::reconcile(&mut conn, &run.job, run.epoch, &run.catalog)
            .await
            .unwrap();
        assert_eq!(run.job.state, JobState::CommitOutcomeUnknown);
    }
    run.expose_publication(&mut conn).await;
    expose_connection(&mut conn, &mut run).await;
    sqlx::query("UPDATE catalog_connections SET enabled=false WHERE id=$1")
        .bind(run.tenant.connection.as_uuid())
        .execute(&mut *conn)
        .await
        .unwrap();
    let never_send = run
        .server
        .mock("POST", TABLE_PATH)
        .expect(0)
        .create_async()
        .await;
    let catalogs = Arc::new(RuntimeCatalogs::new(
        Database::from_pool(pool.clone()),
        RuntimeProfiles {
            default: bergsmith_storage::StorageProfiles::default(),
            connections: std::collections::BTreeMap::new(),
        },
    ));
    let access = RuntimeWorkerAccess::new(catalogs);
    drop(conn);
    access
        .recover_publication(&pool, &run.job, run.epoch)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    run.refresh(&mut conn).await;
    assert_eq!(run.job.state, JobState::Succeeded);
    assert_eq!(run.barrier(&mut conn).await, 0);
    never_send.assert_async().await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn disabled_connections_allow_runtime_recovery_of_committing_jobs(pool: PgPool) {
    disabled_runtime_recovery(pool, false).await;
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn disabled_connections_allow_runtime_recovery_of_unknown_outcomes(pool: PgPool) {
    disabled_runtime_recovery(pool, true).await;
}

fn attestation(run: &Run, revision: i64, reason: &str) -> operator::NonPublicationAttestation {
    operator::NonPublicationAttestation {
        organization: "acme".into(),
        actor: run.tenant.user,
        job: run.job.id,
        revision,
        reason: reason.into(),
    }
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_attestation_command_resolves_an_unknown_outcome_with_an_audit_record(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let run = unknown_outcome(&mut conn).await;
    let revision = run.job.state_revision;

    let error = operator::attest_non_publication(&mut conn, &attestation(&run, revision, "  "))
        .await
        .unwrap_err();
    assert!(matches!(error, OperatorError::InvalidReason), "{error}");
    let long = "x".repeat(operator::MAX_ATTESTATION_REASON_CHARS + 1);
    let error = operator::attest_non_publication(&mut conn, &attestation(&run, revision, &long))
        .await
        .unwrap_err();
    assert!(matches!(error, OperatorError::InvalidReason), "{error}");
    let error =
        operator::attest_non_publication(&mut conn, &attestation(&run, revision - 1, "INC-1"))
            .await
            .unwrap_err();
    assert!(
        matches!(error, OperatorError::NotInspectedOutcome { inspected, .. } if inspected == revision - 1),
        "{error}"
    );
    let viewer = support::user(&mut conn, run.tenant.organization, "bob").await;
    let mut refused = attestation(&run, revision, "INC-1");
    refused.actor = viewer;
    let error = operator::attest_non_publication(&mut conn, &refused)
        .await
        .unwrap_err();
    assert!(matches!(error, OperatorError::Authorization(_)), "{error}");
    assert_eq!(
        current(&pool, &run).await.state,
        JobState::CommitOutcomeUnknown
    );

    let job = operator::attest_non_publication(&mut conn, &attestation(&run, revision, " INC-1 "))
        .await
        .unwrap();

    assert_eq!(job.state, JobState::Failed);
    assert_eq!(run.barrier(&mut conn).await, 0);
    let last = jobs::job_events(&mut conn, run.job.id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(last.actor, Some(run.tenant.user));
    let (actor, detail): (uuid::Uuid, serde_json::Value) = sqlx::query_as(
        "SELECT actor_principal_id, detail FROM audit_events
         WHERE action = 'job.non_publication_attested' AND resource_id = $1",
    )
    .bind(run.job.id.to_string())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(actor, *run.tenant.user.as_uuid());
    assert_eq!(
        detail,
        json!({"revision": revision, "state": "Failed", "reason": "INC-1"})
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_attestation_command_lets_matching_evidence_win(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let mut run = unknown_outcome(&mut conn).await;
    run.expose_publication(&mut conn).await;

    let job = operator::attest_non_publication(
        &mut conn,
        &attestation(&run, run.job.state_revision, "INC-2"),
    )
    .await
    .unwrap();

    assert_eq!(job.state, JobState::Succeeded);
    let detail: serde_json::Value = sqlx::query_scalar(
        "SELECT detail FROM audit_events
         WHERE action = 'job.non_publication_attested' AND resource_id = $1",
    )
    .bind(run.job.id.to_string())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(
        detail,
        json!({"revision": run.job.state_revision, "state": "Succeeded", "reason": "INC-2"})
    );
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn a_failed_audit_record_leaves_the_attested_job_unresolved(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let run = unknown_outcome(&mut conn).await;
    sqlx::raw_sql(
        "CREATE FUNCTION refuse_audit() RETURNS trigger LANGUAGE plpgsql
             AS $$ BEGIN RAISE EXCEPTION 'audit unavailable'; END $$;
         CREATE TRIGGER refuse_audit BEFORE INSERT ON audit_events
             FOR EACH ROW EXECUTE FUNCTION refuse_audit();",
    )
    .execute(&mut *conn)
    .await
    .unwrap();

    let error = operator::attest_non_publication(
        &mut conn,
        &attestation(&run, run.job.state_revision, "INC-4"),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, OperatorError::Publication(_)), "{error}");
    let job = current(&pool, &run).await;
    assert_eq!(job.state, JobState::CommitOutcomeUnknown);
    assert_eq!(job.state_revision, run.job.state_revision);
    assert_eq!(run.barrier(&mut conn).await, 1);
}

#[sqlx::test(migrator = "bergsmith_persistence::MIGRATOR")]
async fn the_attestation_command_needs_a_live_coordinator(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let run = unknown_outcome(&mut conn).await;
    let holder: String = sqlx::query_scalar("SELECT holder FROM coordinator_leadership")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    bergsmith_persistence::leadership::release_leadership(&mut conn, &holder, run.epoch)
        .await
        .unwrap();

    let error = operator::attest_non_publication(
        &mut conn,
        &attestation(&run, run.job.state_revision, "INC-3"),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, OperatorError::NoLiveLeader), "{error}");
    assert_eq!(
        current(&pool, &run).await.state,
        JobState::CommitOutcomeUnknown
    );
}
