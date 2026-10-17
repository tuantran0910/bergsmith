//! Candidates are grouped by spec and partition and packed into bins.

// clippy's `allow-unwrap-in-tests` covers only `#[test]` functions; the
// helpers fail the test on setup errors.
#![expect(clippy::unwrap_used, reason = "test helpers fail the test")]

mod support;

use bergsmith_fixtures::{Commit, Fixture, TableSpec, Value};
use bergsmith_maintenance::{PlannerConfig, Preview};
use iceberg::spec::UnboundPartitionSpec;
use support::{by_tag, data, preview_with, tagged};

/// A bin's spec ID, partition values, and file count.
type Group = (i32, Vec<Option<Vec<u8>>>, u64);

async fn preview(fixture: &Fixture, config: PlannerConfig) -> Preview {
    preview_with(fixture, fixture.metadata(), config, &[])
        .await
        .unwrap()
}

/// Two partitions of the original spec, then an unpartitioned spec.
async fn evolved() -> Fixture {
    TableSpec::new(tagged())
        .partitioned_by(by_tag())
        .commit(
            Commit::new()
                .file(data("y1", 2).partition([Value::string("y")]))
                .file(data("x1", 2).partition([Value::string("x")]))
                .file(data("x2", 3).partition([Value::string("x")])),
        )
        .commit(
            Commit::new()
                .evolve_spec(UnboundPartitionSpec::builder().build())
                .file(data("p1", 2))
                .file(data("p2", 2)),
        )
        .generate()
        .await
        .unwrap()
}

#[tokio::test]
async fn bins_hold_one_spec_and_partition_in_key_order() {
    let fixture = evolved().await;

    let preview = preview(&fixture, PlannerConfig::default()).await;

    let groups: Vec<Group> = preview
        .bins
        .iter()
        .map(|bin| {
            let values = bin
                .partition
                .values()
                .iter()
                .map(|value| value.bytes().map(<[u8]>::to_vec))
                .collect();
            (bin.partition.spec_id(), values, bin.input.files)
        })
        .collect();
    assert_eq!(
        groups,
        [
            (0, vec![Some(b"x".to_vec())], 2),
            (0, vec![Some(b"y".to_vec())], 1),
            (1, vec![], 2),
        ]
    );
    assert_eq!(
        preview.bins.iter().map(|bin| bin.input.files).sum::<u64>(),
        preview.candidates.files
    );
}

#[tokio::test]
async fn bins_respect_the_group_input_limit() {
    let fixture = TableSpec::new(tagged())
        .commit(
            Commit::new()
                .file(data("a", 1))
                .file(data("b", 1))
                .file(data("c", 1)),
        )
        .generate()
        .await
        .unwrap();
    let size = std::fs::metadata(
        fixture
            .path_of("a")
            .unwrap()
            .strip_prefix("file://")
            .unwrap(),
    )
    .unwrap()
    .len();
    let limit = size * 2;

    let preview = preview(
        &fixture,
        PlannerConfig {
            target_file_size_bytes: limit,
            min_file_size_bytes: limit,
            max_group_input_bytes: limit,
            ..PlannerConfig::default()
        },
    )
    .await;

    assert_eq!(preview.bins.len(), 2);
    assert!(preview.bins.iter().all(|bin| bin.input.bytes <= limit));
    assert_eq!(
        preview
            .bins
            .iter()
            .map(|bin| bin.ordinal)
            .collect::<Vec<_>>(),
        [0, 1]
    );
}
