//! The benefit threshold and job limits decide which bins become tasks.

// clippy's `allow-unwrap-in-tests` covers only `#[test]` functions; the
// helpers fail the test on setup errors.
#![expect(clippy::unwrap_used, reason = "test helpers fail the test")]

mod support;

use bergsmith_fixtures::{Commit, FileSpec, Fixture, TableSpec, Value, row};
use bergsmith_maintenance::{BinDisposition, DeferReason, PlannerConfig, Preview};
use support::{ID, by_tag, data, preview_with, tagged};

async fn preview(fixture: &Fixture, config: PlannerConfig) -> Preview {
    preview_with(fixture, fixture.metadata(), config, &[])
        .await
        .unwrap()
}

/// Partition `x` has five small files, `y` two, and `z` one with a delete.
async fn mixed() -> Fixture {
    let mut commit = Commit::new();
    for index in 0..5 {
        commit = commit.file(data(&format!("x{index}"), 2).partition([Value::string("x")]));
    }
    commit = commit
        .file(data("y0", 2).partition([Value::string("y")]))
        .file(data("y1", 2).partition([Value::string("y")]))
        .file(data("z0", 2).partition([Value::string("z")]));
    TableSpec::new(tagged())
        .partitioned_by(by_tag())
        .commit(commit)
        .commit(
            Commit::new().file(
                FileSpec::equality_deletes("e", [ID], [row![ID => 0_i64]])
                    .partition([Value::string("z")]),
            ),
        )
        .generate()
        .await
        .unwrap()
}

#[tokio::test]
async fn only_beneficial_bins_are_admitted() {
    let fixture = mixed().await;

    let preview = preview(&fixture, PlannerConfig::default()).await;

    let decided: Vec<(u64, bool, u64, BinDisposition)> = preview
        .bins
        .iter()
        .map(|bin| {
            (
                bin.input.files,
                bin.has_deletes,
                bin.files_removed,
                bin.disposition,
            )
        })
        .collect();
    assert_eq!(
        decided,
        [
            (5, false, 4, BinDisposition::Admitted),
            (2, false, 1, BinDisposition::NoBenefit),
            (1, true, 0, BinDisposition::Admitted),
        ]
    );
    assert_eq!(
        (
            preview.estimate.tasks,
            preview.estimate.files,
            preview.estimate.files_removed
        ),
        (2, 6, 4)
    );
    assert!(preview.estimate.duration_millis >= 2_000);
}

#[tokio::test]
async fn limits_defer_bins_to_later_jobs() {
    let fixture = mixed().await;

    let preview = preview(
        &fixture,
        PlannerConfig {
            max_job_files: 5,
            ..PlannerConfig::default()
        },
    )
    .await;

    let dispositions: Vec<BinDisposition> =
        preview.bins.iter().map(|bin| bin.disposition).collect();
    assert_eq!(
        dispositions,
        [
            BinDisposition::Admitted,
            BinDisposition::NoBenefit,
            BinDisposition::Deferred(DeferReason::JobFiles),
        ]
    );
}

#[tokio::test]
async fn a_budget_shorter_than_one_task_admits_nothing() {
    let fixture = mixed().await;

    let preview = preview(
        &fixture,
        PlannerConfig {
            duration_budget_millis: 1_000,
            ..PlannerConfig::default()
        },
    )
    .await;

    assert_eq!(preview.estimate.tasks, 0);
    assert!(preview.bins.iter().all(|bin| matches!(
        bin.disposition,
        BinDisposition::NoBenefit | BinDisposition::Deferred(DeferReason::Duration)
    )));
}
