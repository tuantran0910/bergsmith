//! A due job waits for a new commit, within a bound.

// clippy's `allow-unwrap-in-tests` covers only `#[test]` functions; the
// helpers fail the test on setup errors.
#![expect(clippy::unwrap_used, reason = "test helpers fail the test")]

mod support;

use std::sync::Mutex;
use std::time::Duration;

use bergsmith_fixtures::{Commit, TableSpec};
use bergsmith_maintenance::{
    AlignedStart, Alignment, AlignmentConfig, HeadObserver, ObservedHead, align_start,
};
use support::{data, tagged};
use tokio::time::Instant;

/// Returns the scripted heads in order, then repeats the last one, and
/// records when each observation happened.
struct Script {
    heads: Mutex<Vec<Result<ObservedHead, &'static str>>>,
    observed_at: Mutex<Vec<Instant>>,
}

impl Script {
    fn new(heads: Vec<Result<ObservedHead, &'static str>>) -> Self {
        Self {
            heads: Mutex::new(heads.into_iter().rev().collect()),
            observed_at: Mutex::new(Vec::new()),
        }
    }

    fn observations(&self) -> usize {
        self.observed_at.lock().unwrap().len()
    }
}

impl HeadObserver for Script {
    type Error = &'static str;

    async fn observe(&self) -> Result<ObservedHead, &'static str> {
        self.observed_at.lock().unwrap().push(Instant::now());
        let mut heads = self.heads.lock().unwrap();
        if heads.len() > 1 {
            heads.pop().unwrap()
        } else {
            heads.last().unwrap().clone()
        }
    }
}

/// Heads with no snapshot, one, and two.
async fn heads() -> [ObservedHead; 3] {
    let mut table = TableSpec::new(tagged());
    let mut heads = Vec::new();
    for index in 0..3 {
        let fixture = table.generate().await.unwrap();
        heads.push(ObservedHead {
            metadata: fixture.metadata().clone(),
            metadata_location: Some(fixture.metadata_location().to_owned()),
        });
        table = table.commit(Commit::new().file(data(&format!("d{index}"), 1)));
    }
    heads.try_into().unwrap()
}

fn config() -> AlignmentConfig {
    AlignmentConfig {
        max_wait: Duration::from_secs(60),
        poll_interval: Duration::from_secs(10),
    }
}

async fn align(script: &Script) -> Result<AlignedStart, &'static str> {
    align_start(script, &config()).await
}

#[tokio::test(start_paused = true)]
async fn the_job_starts_right_after_a_new_commit() {
    let [_, one, two] = heads().await;
    let script = Script::new(vec![Ok(one.clone()), Ok(one), Ok(two.clone())]);
    let start = Instant::now();

    let aligned = align(&script).await.unwrap();

    assert_eq!(aligned.alignment, Alignment::NewCommit);
    assert_eq!(aligned.head, two);
    assert_eq!(start.elapsed(), Duration::from_secs(20));
}

#[tokio::test(start_paused = true)]
async fn a_first_commit_counts_as_new() {
    let [empty, one, _] = heads().await;
    let script = Script::new(vec![Ok(empty), Ok(one.clone())]);

    let aligned = align(&script).await.unwrap();

    assert_eq!(aligned.alignment, Alignment::NewCommit);
    assert_eq!(aligned.head.main_snapshot_id(), Some(1));
}

#[tokio::test(start_paused = true)]
async fn a_quiet_table_proceeds_when_the_wait_elapses() {
    let [_, one, _] = heads().await;
    let script = Script::new(vec![Ok(one.clone())]);
    let start = Instant::now();

    let aligned = align(&script).await.unwrap();

    assert_eq!(aligned.alignment, Alignment::WaitElapsed);
    assert_eq!(aligned.head, one);
    assert_eq!(start.elapsed(), Duration::from_secs(60));
    assert_eq!(script.observations(), 7, "one at start, then every 10 s");
}

#[tokio::test(start_paused = true)]
async fn the_last_poll_is_cut_to_the_deadline() {
    let [_, one, _] = heads().await;
    let script = Script::new(vec![Ok(one)]);
    let start = Instant::now();

    let aligned = align_start(
        &script,
        &AlignmentConfig {
            max_wait: Duration::from_secs(25),
            poll_interval: Duration::from_secs(10),
        },
    )
    .await
    .unwrap();

    assert_eq!(aligned.alignment, Alignment::WaitElapsed);
    assert_eq!(start.elapsed(), Duration::from_secs(25));
    assert_eq!(script.observations(), 4);
}

#[tokio::test(start_paused = true)]
async fn no_wait_plans_against_the_current_head() {
    let [_, one, _] = heads().await;
    let script = Script::new(vec![Ok(one.clone())]);

    let aligned = align_start(
        &script,
        &AlignmentConfig {
            max_wait: Duration::ZERO,
            ..config()
        },
    )
    .await
    .unwrap();

    assert_eq!(aligned.alignment, Alignment::WaitElapsed);
    assert_eq!(aligned.head, one);
    assert_eq!(script.observations(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_observation_failure_ends_the_wait() {
    let [_, one, _] = heads().await;
    let script = Script::new(vec![Ok(one), Err("catalog unavailable")]);

    assert_eq!(align(&script).await.unwrap_err(), "catalog unavailable");
    assert_eq!(script.observations(), 2);
}

#[test]
fn the_default_wait_covers_one_commit_interval() {
    let config = AlignmentConfig::default();

    assert_eq!(config.max_wait, Duration::from_secs(360));
    assert_eq!(config.poll_interval, Duration::from_secs(5));
}
