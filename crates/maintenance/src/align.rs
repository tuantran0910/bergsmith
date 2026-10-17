//! Commit-aligned start (BS-009.T5, ADR 0010).
//!
//! Under conservative conflict rejection a job succeeds only if it fits
//! between two commits of the table's writers. A due job therefore waits,
//! for a bounded time, until a new `main` snapshot appears, and plans
//! against it. If the wait elapses the job plans against the latest head
//! anyway: a table with no new commits is quiet, the best case.

use std::future::Future;
use std::time::Duration;

use iceberg::spec::{MAIN_BRANCH, TableMetadata};
use tokio::time::Instant;

/// How long and how often to look for a new commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlignmentConfig {
    /// The longest a job waits for a new commit. The default, 6 minutes, is
    /// one 5-minute commit interval plus slack.
    pub max_wait: Duration,
    /// Time between observations.
    pub poll_interval: Duration,
}

impl Default for AlignmentConfig {
    fn default() -> Self {
        Self {
            max_wait: Duration::from_secs(6 * 60),
            poll_interval: Duration::from_secs(5),
        }
    }
}

/// A table as one observation loaded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedHead {
    /// The loaded metadata.
    pub metadata: TableMetadata,
    /// The metadata file it was read from; `None` for a staged table.
    pub metadata_location: Option<String>,
}

impl ObservedHead {
    /// The `main` snapshot, if the table has one.
    #[must_use]
    pub fn main_snapshot_id(&self) -> Option<i64> {
        self.metadata
            .snapshot_for_ref(MAIN_BRANCH)
            .map(|snapshot| snapshot.snapshot_id())
    }
}

/// Loads the current table, such as through the catalog.
pub trait HeadObserver {
    /// Why an observation failed.
    type Error;

    /// Loads the table as it is now.
    fn observe(&self) -> impl Future<Output = Result<ObservedHead, Self::Error>> + Send;
}

/// How the wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alignment {
    /// A new `main` snapshot appeared.
    NewCommit,
    /// No new snapshot appeared within the wait.
    WaitElapsed,
}

/// The head to plan against, and how it was reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlignedStart {
    /// The latest observation.
    pub head: ObservedHead,
    /// How the wait ended.
    pub alignment: Alignment,
}

/// Observes the table, then polls until `main` points to a different
/// snapshot or `config.max_wait` elapses, and returns the latest head.
///
/// # Errors
///
/// The observer's error, from the first observation that fails. The wait
/// does not retry: the caller decides whether a failure is transient.
pub async fn align_start<O: HeadObserver>(
    observer: &O,
    config: &AlignmentConfig,
) -> Result<AlignedStart, O::Error> {
    let deadline = Instant::now() + config.max_wait;
    let mut head = observer.observe().await?;
    let initial = head.main_snapshot_id();
    let poll = config.poll_interval.max(Duration::from_millis(1));
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(AlignedStart {
                head,
                alignment: Alignment::WaitElapsed,
            });
        }
        tokio::time::sleep(poll.min(deadline - now)).await;
        head = observer.observe().await?;
        if head.main_snapshot_id() != initial {
            return Ok(AlignedStart {
                head,
                alignment: Alignment::NewCommit,
            });
        }
    }
}
