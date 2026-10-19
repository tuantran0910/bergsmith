use super::checkpoints::checkpoint_sync;
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use bergsmith_catalog::{Baseline, CatalogConnection, ConnectionConfig, LoadedTable, TableName};
use bergsmith_coordinator::durable_publication;
use bergsmith_fixtures::live_rows;
use bergsmith_persistence::{PersistenceError, cancellation, publications};
use iceberg::spec::TableMetadata;
use mockito::{Server, ServerGuard};

const TABLE_PATH: &str = "/v1/namespaces/db/tables/native";

pub(super) struct Catalog {
    server: ServerGuard,
    metadata: Arc<Mutex<TableMetadata>>,
    posts: Arc<AtomicUsize>,
    response: Arc<(Mutex<bool>, Condvar)>,
}

impl Catalog {
    pub(super) async fn new(fixture: &Fixture, directory: &Path) -> Self {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/v1/config")
            .with_body(r#"{"defaults":{},"overrides":{}}"#)
            .create_async()
            .await;
        let metadata = Arc::new(Mutex::new(fixture.metadata().clone()));
        let current = metadata.clone();
        let location = fixture.metadata_location().to_owned();
        server.mock("GET", TABLE_PATH).with_body_from_request(move |_| {
            serde_json::to_vec(&serde_json::json!({"metadata-location": location, "metadata": *current.lock().unwrap()})).unwrap()
        }).create_async().await;
        let response = Arc::new((Mutex::new(false), Condvar::new()));
        let gate = response.clone();
        let directory = directory.to_owned();
        let posts = Arc::new(AtomicUsize::new(0));
        let count = posts.clone();
        let current = metadata.clone();
        server.mock("POST", TABLE_PATH).with_status(500).with_body_from_request(move |request| {
            count.fetch_add(1, Ordering::AcqRel);
            let request = bergsmith_catalog::CommitRequest::from_persisted(
                TableName {namespace: vec!["db".into()], name: "native".into()},
                request.body().unwrap().clone(),
            ).decode().unwrap();
            {
            let mut metadata = current.lock().unwrap();
            for requirement in request.requirements {requirement.check(Some(&metadata)).unwrap();}
            let mut builder = metadata.clone().into_builder(None);
            for update in request.updates {builder = update.apply(builder).unwrap();}
            *metadata = builder.build().unwrap().metadata;
            }
            checkpoint_sync(&directory, "commit-applied");
            // Hold the HTTP response until the publishing process is confirmed
            // dead. It cannot record an outcome or reconcile its own evidence.
            let (released, changed) = &*gate;
            let mut released = released.lock().unwrap();
            while !*released { released = changed.wait(released).unwrap(); }
            // The commit succeeds in the catalog but its successful response is
            // replaced with an ambiguous server failure. Recovery must only read.
            br#"{"error":{"message":"injected lost commit response","type":"InternalServerError","code":500}}"#.to_vec()
        }).create_async().await;
        Self {
            server,
            metadata,
            posts,
            response,
        }
    }

    fn release_response(&self) {
        let (released, changed) = &*self.response;
        *released.lock().unwrap() = true;
        changed.notify_all();
    }

    pub(super) fn url(&self) -> String {
        self.server.url()
    }
}

impl Drop for Catalog {
    fn drop(&mut self) {
        // Unblock the synchronous mock callback on assertion failure too.
        self.release_response();
    }
}

impl Harness {
    async fn prepare_intent(&self) -> (CatalogConnection, jobs::Job, i64) {
        let catalog = CatalogConnection::connect(&ConnectionConfig::new(
            url::Url::parse(&self.catalog.url()).unwrap(),
        ))
        .await
        .unwrap();
        let baseline = Baseline::new(
            &LoadedTable {
                name: TableName {
                    namespace: vec!["db".into()],
                    name: "native".into(),
                },
                metadata_location: Some(self.fixture.metadata_location().into()),
                metadata: self.fixture.metadata().clone(),
                location: Location::parse(self.fixture.metadata().location()).unwrap(),
                credentials: vec![],
            },
            self.fixture.file_io().clone(),
        )
        .unwrap();
        let ready: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.path("ready.json")).unwrap()).unwrap();
        let epoch = ready["epoch"].as_i64().unwrap();
        let mut conn = self.pool.acquire().await.unwrap();
        let job = jobs::job(&mut conn, self.tenant.organization, self.job)
            .await
            .unwrap();
        let committing = durable_publication::prepare_publication(
            &mut conn,
            &job,
            epoch,
            self.tenant.connection,
            &catalog,
            &baseline,
            &self.store,
        )
        .await
        .unwrap();
        (catalog, committing, epoch)
    }

    pub(crate) async fn publish_and_recover(&mut self) {
        let before = self.fixture.live_rows().await.unwrap();
        let (catalog, committing, epoch) = self.prepare_intent().await;
        let intent = publications::intent(&mut self.pool.acquire().await.unwrap(), self.job)
            .await
            .unwrap();
        self.command("publish").await;
        self.wait_file("commit-applied").await;
        let mut child = self.coordinator.take().unwrap();
        child.kill().await.unwrap();
        assert!(!child.wait().await.unwrap().success());
        // This is an authorized, unresolved send, not a cleanly recorded 500.
        let mut conn = self.pool.acquire().await.unwrap();
        let unresolved = jobs::job(&mut conn, self.tenant.organization, self.job)
            .await
            .unwrap();
        assert_eq!(unresolved.state, JobState::Committing);
        assert!(unresolved.publication_started_at.is_some());
        assert_eq!(unresolved.state_revision, committing.state_revision);
        assert_eq!(self.catalog.posts.load(Ordering::Acquire), 1);
        drop(conn);
        self.catalog.release_response();
        self.start_coordinator().await;
        let mut conn = self.pool.acquire().await.unwrap();
        let succeeded = jobs::job(&mut conn, self.tenant.organization, self.job)
            .await
            .unwrap();
        assert_eq!(succeeded.state, JobState::Succeeded);
        let recovered = publications::intent(&mut conn, self.job).await.unwrap();
        assert_eq!(intent.request_body, recovered.request_body);
        assert_eq!(intent.publication_id, recovered.publication_id);
        assert!(matches!(
            durable_publication::publish(&mut conn, &succeeded, epoch, &catalog)
                .await
                .unwrap_err(),
            durable_publication::DurablePublicationError::Persistence(
                PersistenceError::PublicationAlreadyAuthorized { .. }
            )
        ));
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM publications WHERE job_id=$1")
            .bind(self.job.as_uuid())
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(self.catalog.posts.load(Ordering::Acquire), 1);
        let published = self.catalog.metadata.lock().unwrap().clone();
        assert_eq!(
            published.snapshots().count(),
            self.fixture.metadata().snapshots().count() + 1
        );
        assert_eq!(
            live_rows(&published, self.fixture.file_io(), None)
                .await
                .unwrap(),
            before
        );
    }

    pub(crate) async fn cancel_intent_and_recover(&mut self) {
        let (catalog, committing, epoch) = self.prepare_intent().await;
        assert!(committing.publication_started_at.is_none());
        // Stop periodic reconciliation before recording the pending command.
        // Startup must reconstruct it entirely from PostgreSQL.
        let mut child = self.coordinator.take().unwrap();
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        let mut conn = self.pool.acquire().await.unwrap();
        let request = cancellation::CancelJob {
            job_id: self.job,
            requested_by: None,
            reason: Some("process acceptance".into()),
        };
        let pending = cancellation::request_cancellation(&mut conn, &request)
            .await
            .unwrap();
        assert_eq!(
            pending.disposition,
            cancellation::CancellationDisposition::Cancelling
        );
        assert!(
            durable_publication::publish(&mut conn, &pending.job, epoch, &catalog)
                .await
                .is_err()
        );
        drop(conn);
        self.start_coordinator().await;
        let mut conn = self.pool.acquire().await.unwrap();
        let cancelled = jobs::job(&mut conn, self.tenant.organization, self.job)
            .await
            .unwrap();
        assert_eq!(cancelled.state, JobState::Cancelled);
        assert!(cancelled.publication_started_at.is_none());
        let replay = cancellation::request_cancellation(&mut conn, &request)
            .await
            .unwrap();
        assert_eq!(
            replay.disposition,
            cancellation::CancellationDisposition::AlreadyEnded(JobState::Cancelled)
        );
        assert_eq!(replay.job, cancelled);
        assert_eq!(self.catalog.posts.load(Ordering::Acquire), 0);
        assert_eq!(
            *self.catalog.metadata.lock().unwrap(),
            *self.fixture.metadata()
        );
        let barriers: i64 =
            sqlx::query_scalar("SELECT count(*) FROM publication_barriers WHERE job_id=$1")
                .bind(self.job.as_uuid())
                .fetch_one(&mut *conn)
                .await
                .unwrap();
        assert_eq!(barriers, 0);
    }
}
