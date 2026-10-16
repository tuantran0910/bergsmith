//! The conflict-rate harness against the Compose Lakekeeper (BS-007.T6).
//!
//! These tests place synthetic sink commits between a model job's phases
//! deterministically, to show that each placement has the outcome the
//! measurement counts. `examples/conflict_rate.rs` runs the same harness
//! against wall-clock cadences.

mod support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use bergsmith_catalog::{BaselineChange, CatalogConnection, TableName};
use support::Warehouse;
use support::sink::{JobResult, MaintenanceJob, SyntheticSink, next_main};

struct Fixture {
    _warehouse: Warehouse,
    jobs: CatalogConnection,
    name: TableName,
    sink: Arc<SyntheticSink>,
}

/// A table the sink has committed to once, and a separate connection for
/// maintenance.
async fn fixture() -> Fixture {
    let warehouse = Warehouse::create().await;
    let name = warehouse.create_table("db", "events").await;
    let sink = SyntheticSink::new(warehouse.connect().await, name.clone());
    sink.commit_once().await;
    Fixture {
        jobs: warehouse.connect().await,
        _warehouse: warehouse,
        name,
        sink,
    }
}

#[tokio::test]
async fn a_job_between_sink_commits_publishes() {
    let fixture = fixture().await;
    let mut rewritten = BTreeSet::new();

    let job = MaintenanceJob::plan(&fixture.jobs, &fixture.sink, &rewritten)
        .await
        .unwrap();
    assert_eq!(job.preflight(&fixture.jobs).await, []);
    let result = job.send(&fixture.jobs, &mut rewritten).await;
    fixture.sink.commit_once().await;

    assert_eq!(result, JobResult::Published);
    assert_eq!(rewritten.len(), 1);
    assert_eq!(fixture.sink.retries().await, 0);
}

#[tokio::test]
async fn a_sink_commit_during_execution_is_caught_by_the_reload_check() {
    let fixture = fixture().await;
    let rewritten = BTreeSet::new();
    let job = MaintenanceJob::plan(&fixture.jobs, &fixture.sink, &rewritten)
        .await
        .unwrap();

    fixture.sink.commit_once().await;
    let changes = job.preflight(&fixture.jobs).await;

    assert!(
        changes
            .iter()
            .any(|change| matches!(change, BaselineChange::MainSnapshot { .. })),
        "{changes:?}"
    );
}

#[tokio::test]
async fn a_sink_commit_after_the_reload_check_is_rejected_by_the_catalog() {
    let fixture = fixture().await;
    let mut rewritten = BTreeSet::new();
    let job = MaintenanceJob::plan(&fixture.jobs, &fixture.sink, &rewritten)
        .await
        .unwrap();
    assert_eq!(job.preflight(&fixture.jobs).await, []);

    fixture.sink.commit_once().await;
    let result = job.send(&fixture.jobs, &mut rewritten).await;

    assert_eq!(result, JobResult::Rejected);
    assert!(rewritten.is_empty());
}

#[tokio::test]
async fn successive_jobs_rewrite_different_sink_files() {
    let fixture = fixture().await;
    let mut rewritten = BTreeSet::new();

    let first =
        MaintenanceJob::run(&fixture.jobs, &fixture.sink, Duration::ZERO, &mut rewritten).await;
    let exhausted = MaintenanceJob::plan(&fixture.jobs, &fixture.sink, &rewritten).await;
    fixture.sink.commit_once().await;
    let second =
        MaintenanceJob::run(&fixture.jobs, &fixture.sink, Duration::ZERO, &mut rewritten).await;

    assert_eq!(first, Some(JobResult::Published));
    assert!(
        exhausted.is_none(),
        "the only sink file was already rewritten"
    );
    assert_eq!(second, Some(JobResult::Published));
    assert_eq!(rewritten.len(), 2);
}

#[tokio::test]
async fn jobs_start_when_inventory_observes_a_new_main_snapshot() {
    let fixture = fixture().await;
    let before = next_main(&fixture.jobs, &fixture.name, None, Duration::ZERO).await;

    fixture.sink.commit_once().await;
    let after = next_main(&fixture.jobs, &fixture.name, before, Duration::ZERO).await;

    assert!(before.is_some());
    assert!(after.is_some());
    assert_ne!(after, before);
}
