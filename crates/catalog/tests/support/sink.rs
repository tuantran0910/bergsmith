//! A synthetic streaming sink and a model maintenance job, for measuring
//! how often conservative commits conflict with a steady writer (BS-007.T6).
//!
//! The sink appends one data file per commit and, like a streaming sink,
//! retries after a conflict. A job pins a baseline, rewrites one sink file
//! that is live in it, rechecks the baseline, and sends the commit. Each
//! phase is a separate call so tests can place writer commits between them.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bergsmith_catalog::{
    Baseline, BaselineChange, CatalogConnection, CommitOutcome, PreparedCommit, RejectionKind,
    TableName,
};
use iceberg::spec::{DataFile, MAIN_BRANCH};
use iceberg::transaction::ApplyTransactionAction;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::commit::{baseline, data_file, rewrite};

/// A file the sink committed, with the sequence number it was committed at.
#[derive(Clone)]
struct SinkFile {
    sequence_number: i64,
    file: DataFile,
}

/// A writer that appends a file per commit.
pub(crate) struct SyntheticSink {
    connection: CatalogConnection,
    name: TableName,
    files: Mutex<Vec<SinkFile>>,
    /// Commits retried after a conflict with maintenance.
    retries: Mutex<u64>,
}

impl SyntheticSink {
    pub(crate) fn new(connection: CatalogConnection, name: TableName) -> Arc<Self> {
        Arc::new(Self {
            connection,
            name,
            files: Mutex::new(Vec::new()),
            retries: Mutex::new(0),
        })
    }

    /// Appends one file, retrying on a requirement conflict until it lands.
    pub(crate) async fn commit_once(&self) {
        let index = self.files.lock().await.len();
        loop {
            let pinned = baseline(&self.connection, &self.name).await;
            let file = data_file(&pinned, &format!("sink-{index:05}"), 100);
            let request = pinned
                .prepare_with(|transaction| {
                    transaction
                        .fast_append()
                        .add_data_files([file.clone()])
                        .apply(transaction)
                })
                .await
                .expect("the append builds");
            match self
                .connection
                .commit(&request, None)
                .await
                .expect("the sink reaches the catalog")
            {
                CommitOutcome::Committed(committed) => {
                    let sequence_number = committed
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.snapshot_for_ref(MAIN_BRANCH))
                        .expect("Lakekeeper returns the new metadata")
                        .sequence_number();
                    self.files.lock().await.push(SinkFile {
                        sequence_number,
                        file,
                    });
                    return;
                }
                CommitOutcome::Rejected(rejection)
                    if rejection.kind == RejectionKind::RequirementFailed =>
                {
                    *self.retries.lock().await += 1;
                }
                other => panic!("the sink commit failed: {other:?}"),
            }
        }
    }

    /// Commits every `interval` until the handle is dropped.
    pub(crate) fn spawn(self: &Arc<Self>, interval: Duration) -> SinkHandle {
        let sink = Arc::clone(self);
        SinkHandle(tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                sink.commit_once().await;
            }
        }))
    }

    /// Sink commits retried because maintenance committed first.
    pub(crate) async fn retries(&self) -> u64 {
        *self.retries.lock().await
    }

    /// The newest sink file live in `baseline` and not yet rewritten.
    async fn newest_live(
        &self,
        baseline: &Baseline,
        rewritten: &BTreeSet<String>,
    ) -> Option<DataFile> {
        let main = baseline.main_snapshot()?.sequence_number();
        self.files
            .lock()
            .await
            .iter()
            .rev()
            .find(|f| f.sequence_number <= main && !rewritten.contains(f.file.file_path()))
            .map(|f| f.file.clone())
    }
}

/// Stops the sink's commit loop when dropped.
pub(crate) struct SinkHandle(JoinHandle<()>);

impl Drop for SinkHandle {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// How a model job ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobResult {
    /// The rewrite was published.
    Published,
    /// The reload check saw the baseline change; nothing was sent.
    ReplanBeforeSend,
    /// The catalog rejected the commit's original requirements.
    Rejected,
}

/// A model maintenance job between its phases.
pub(crate) struct MaintenanceJob {
    baseline: Baseline,
    prepared: PreparedCommit,
    rewritten: String,
}

impl MaintenanceJob {
    /// Pins the current table and prepares a rewrite of the newest sink file
    /// that is still live, or `None` if every file was rewritten.
    pub(crate) async fn plan(
        connection: &CatalogConnection,
        sink: &SyntheticSink,
        rewritten: &BTreeSet<String>,
    ) -> Option<Self> {
        let pinned = baseline(connection, &sink.name).await;
        let source = sink.newest_live(&pinned, rewritten).await?;
        let output = data_file(&pinned, &format!("compacted-{}", uuid::Uuid::now_v7()), 100);
        let prepared = rewrite(&pinned, vec![source.clone()], vec![output])
            .prepare(&pinned)
            .await
            .expect("the rewrite builds");
        Some(Self {
            baseline: pinned,
            prepared,
            rewritten: source.file_path().to_owned(),
        })
    }

    /// The reload check immediately before publication.
    pub(crate) async fn preflight(&self, connection: &CatalogConnection) -> Vec<BaselineChange> {
        let current = connection
            .load_table(self.baseline.name(), false)
            .await
            .expect("the table reloads");
        self.baseline.changes(&current)
    }

    /// Sends the commit and classifies the result. Records the rewritten
    /// file when it was published.
    pub(crate) async fn send(
        &self,
        connection: &CatalogConnection,
        rewritten: &mut BTreeSet<String>,
    ) -> JobResult {
        match connection
            .commit(&self.prepared.request, None)
            .await
            .expect("the job reaches the catalog")
        {
            CommitOutcome::Committed(_) => {
                rewritten.insert(self.rewritten.clone());
                JobResult::Published
            }
            CommitOutcome::Rejected(rejection)
                if rejection.kind == RejectionKind::RequirementFailed =>
            {
                JobResult::Rejected
            }
            other => panic!("the job commit failed: {other:?}"),
        }
    }

    /// Runs a whole job: plan, execute for `duration`, recheck, and send.
    pub(crate) async fn run(
        connection: &CatalogConnection,
        sink: &SyntheticSink,
        duration: Duration,
        rewritten: &mut BTreeSet<String>,
    ) -> Option<JobResult> {
        let job = Self::plan(connection, sink, rewritten).await?;
        tokio::time::sleep(duration).await;
        if !job.preflight(connection).await.is_empty() {
            return Some(JobResult::ReplanBeforeSend);
        }
        Some(job.send(connection, rewritten).await)
    }
}

/// Waits until `main` differs from `last`, polling every `poll`, and
/// returns the new `main` snapshot ID: Inventory observing a sink commit.
pub(crate) async fn next_main(
    connection: &CatalogConnection,
    name: &TableName,
    last: Option<i64>,
    poll: Duration,
) -> Option<i64> {
    loop {
        let main = connection
            .load_table(name, false)
            .await
            .expect("the table loads")
            .metadata
            .snapshot_for_ref(MAIN_BRANCH)
            .map(|snapshot| snapshot.snapshot_id());
        if main != last {
            return main;
        }
        tokio::time::sleep(poll).await;
    }
}
