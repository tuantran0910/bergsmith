//! Native outputs plus PostgreSQL crash points and a controllable REST catalog.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "acceptance fixtures fail on setup errors"
)]

#[path = "../../persistence/tests/support/mod.rs"]
mod support;

use bergsmith_catalog::{Baseline, CatalogConnection, ConnectionConfig, LoadedTable, TableName};
use bergsmith_common::ids::{AttemptId, PlanId};
use bergsmith_common::job::JobState;
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
    let queued = planning::record_outcome(conn, &job, &outcome)
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
    run.job = publication::reconcile(&mut conn, &run.job, &run.catalog)
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
    run.job = publication::reconcile(&mut conn, &pending[0], &run.catalog)
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
    run.job = publication::reconcile(&mut conn, &run.job, &run.catalog)
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
    publications::pend_disposition(&mut conn, run.job.id, JobState::Failed, "ExecutionFailed")
        .await
        .unwrap();
    assert!(
        publications::resolve(
            &mut conn,
            run.job.id,
            run.job.state_revision,
            JobState::Failed,
            "Unproven",
            false
        )
        .await
        .is_err()
    );
    run.expose_publication(&mut conn).await;
    run.job = publication::reconcile(&mut conn, &run.job, &run.catalog)
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
        run.job = publication::reconcile(&mut conn, &run.job, &run.catalog)
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
        publication::reconcile(&mut conn, &run.job, &run.catalog)
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
    run.job = publication::reconcile(&mut conn, &run.job, &run.catalog)
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
    run.job = publication::reconcile(&mut conn, &run.job, &run.catalog)
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
        vec![],
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
