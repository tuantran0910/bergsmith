//! Metadata-only statistics over the native fixtures (BS-025.T1).

// clippy's `allow-unwrap-in-tests` and `allow-panic-in-tests` cover only
// `#[test]` functions; the helpers fail the test on setup errors.
#![expect(clippy::unwrap_used, reason = "test helpers fail the test")]

mod support;

use std::path::PathBuf;

use bergsmith_fixtures::{Commit, FileSpec, Fixture, TableSpec, Value, row};
use bergsmith_maintenance::{StatisticsConfig, StatisticsError, TableSample, snapshot_history};
use iceberg::spec::UnboundPartitionSpec;
use support::{ID, by_tag, data, tagged};

/// Two commits: three data files across two specs, then a position and an
/// equality delete.
async fn mixed() -> Fixture {
    TableSpec::new(tagged())
        .partitioned_by(by_tag())
        .commit(
            Commit::new()
                .file(data("x0", 2).partition([Value::string("x")]))
                .file(data("x1", 2).partition([Value::string("x")])),
        )
        .commit(
            Commit::new()
                .evolve_spec(UnboundPartitionSpec::builder().build())
                .file(data("u0", 2)),
        )
        .commit(
            Commit::new()
                .file(
                    FileSpec::position_deletes("p0", [("x0", 0)])
                        .in_spec(0)
                        .partition([Value::string("x")]),
                )
                .file(
                    FileSpec::equality_deletes("e0", [ID], [row![ID => 1_i64]])
                        .in_spec(0)
                        .partition([Value::string("x")]),
                ),
        )
        .generate()
        .await
        .unwrap()
}

/// The declared file, as written on the local filesystem.
fn fixture_file(fixture: &Fixture, name: &str) -> PathBuf {
    fixture.root().join("data").join(format!("{name}.parquet"))
}

fn size_of(fixture: &Fixture, name: &str) -> u64 {
    std::fs::metadata(fixture_file(fixture, name))
        .unwrap()
        .len()
}

async fn sample(fixture: &Fixture, config: &StatisticsConfig) -> TableSample {
    TableSample::collect(
        fixture.metadata(),
        fixture.metadata_location(),
        fixture.file_io(),
        config,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn counts_live_files_and_delete_kinds_across_specs() {
    let fixture = mixed().await;
    let sample = sample(&fixture, &StatisticsConfig::default()).await;

    assert_eq!(sample.metadata_location, fixture.metadata_location());
    assert_eq!(sample.snapshot_id, Some(3));
    assert_eq!(sample.last_operation.as_deref(), Some("delete"));
    assert_eq!(sample.record_count, 6);
    assert_eq!(sample.data_files, 3);
    assert_eq!(
        sample.data_bytes,
        size_of(&fixture, "x0") + size_of(&fixture, "x1") + size_of(&fixture, "u0")
    );
    assert_eq!(sample.delete_files, 2);
    assert_eq!(sample.position_delete_files, 1);
    assert_eq!(sample.equality_delete_files, 1);
    assert_eq!(sample.position_delete_records, 1);
    assert_eq!(sample.equality_delete_records, 1);
    assert_eq!(
        sample.delete_bytes,
        size_of(&fixture, "p0") + size_of(&fixture, "e0")
    );
    assert!(sample.manifest_count >= 1);
    // Delete files live in delete manifests, which never count as data
    // manifests; this fixture committed data twice and deletes once.
    assert!(sample.data_manifest_count >= 2);
    assert!(
        sample.data_manifest_count < sample.manifest_count,
        "a delete manifest is not a data manifest"
    );
    assert!(sample.manifest_bytes > 0);
    assert_eq!(sample.snapshot_count, 3);
    // The fixture builds every commit into one metadata file, and iceberg-rust
    // keeps only the final snapshot in that build's log.
    assert_eq!(sample.oldest_snapshot_at, sample.newest_snapshot_at);
    assert_eq!(
        sample.newest_snapshot_at.map(|at| at.to_rfc3339()),
        Some("2026-01-01T00:02:00+00:00".to_owned())
    );
}

#[tokio::test]
async fn partitions_are_grouped_by_spec_and_ordered_by_file_count() {
    let fixture = mixed().await;
    let sample = sample(&fixture, &StatisticsConfig::default()).await;

    assert_eq!(sample.partition_count, 2);
    assert_eq!(sample.top_partitions.len(), 2);
    let busiest = &sample.top_partitions[0];
    assert_eq!(busiest.spec_id, 0);
    assert_eq!(busiest.partition, "tag=x");
    assert_eq!(busiest.files, 2);
    assert_eq!(busiest.delete_files, 2);
    assert_eq!(
        busiest.bytes,
        size_of(&fixture, "x0") + size_of(&fixture, "x1")
    );
    let unpartitioned = &sample.top_partitions[1];
    assert_eq!(unpartitioned.spec_id, 1);
    assert_eq!(unpartitioned.partition, "");
    assert_eq!(unpartitioned.files, 1);
    assert_eq!(unpartitioned.delete_files, 0);
    assert_eq!(sample.max_partition_files, 2);
}

#[tokio::test]
async fn a_hidden_partition_list_still_records_the_busiest_count() {
    let fixture = mixed().await;
    let config = StatisticsConfig {
        top_partitions: 0,
        ..StatisticsConfig::default()
    };
    let sample = sample(&fixture, &config).await;

    assert!(sample.top_partitions.is_empty());
    assert_eq!(sample.partition_count, 2);
    assert_eq!(
        sample.max_partition_files, 2,
        "the busiest count survives a zero display bound"
    );
}

#[tokio::test]
async fn top_partitions_are_bounded() {
    let fixture = mixed().await;
    let config = StatisticsConfig {
        top_partitions: 1,
        ..StatisticsConfig::default()
    };
    let sample = sample(&fixture, &config).await;

    assert_eq!(sample.partition_count, 2);
    assert_eq!(sample.top_partitions.len(), 1);
    assert_eq!(sample.top_partitions[0].partition, "tag=x");
    assert_eq!(sample.top_partitions[0].delete_files, 2);
}

#[tokio::test]
async fn delete_only_partitions_are_counted_but_not_listed() {
    // Deletes in a partition with no live data file cannot appear in a list
    // ordered by data-file count, but they are not lost: the totals keep them.
    let fixture = TableSpec::new(tagged())
        .partitioned_by(by_tag())
        .commit(
            Commit::new()
                .file(data("x0", 2).partition([Value::string("x")]))
                .file(
                    FileSpec::position_deletes("p0", [("x0", 0)]).partition([Value::string("y")]),
                ),
        )
        .generate()
        .await
        .unwrap();
    let sample = sample(&fixture, &StatisticsConfig::default()).await;

    assert_eq!(sample.delete_files, 1);
    assert_eq!(sample.partition_count, 1);
    assert_eq!(sample.top_partitions.len(), 1);
    assert_eq!(sample.top_partitions[0].partition, "tag=x");
    assert_eq!(sample.top_partitions[0].delete_files, 0);
}

#[tokio::test]
async fn histogram_buckets_cover_every_data_file_once() {
    let fixture = mixed().await;
    let config = StatisticsConfig {
        size_buckets_bytes: vec![1, 1 << 40],
        ..StatisticsConfig::default()
    };
    let sample = sample(&fixture, &config).await;

    assert_eq!(sample.size_histogram.len(), 3);
    let counted: u64 = sample
        .size_histogram
        .iter()
        .map(|bucket| bucket.files)
        .sum();
    assert_eq!(counted, sample.data_files);
    let bytes: u64 = sample
        .size_histogram
        .iter()
        .map(|bucket| bucket.bytes)
        .sum();
    assert_eq!(bytes, sample.data_bytes);
    // Every fixture file is at least one byte and far below a tebibyte.
    assert_eq!(sample.size_histogram[0].files, 0);
    assert_eq!(sample.size_histogram[1].files, 3);
    assert_eq!(sample.size_histogram[2].files, 0);
}

#[tokio::test]
async fn small_files_use_the_table_target_over_the_default() {
    let fixture = mixed().await;
    let small = sample(&fixture, &StatisticsConfig::default()).await;
    assert_eq!(small.target_file_size_bytes, 512 * 1024 * 1024);
    assert_eq!(small.small_files, small.data_files);

    let metadata =
        support::with_properties(fixture.metadata(), &[("write.target-file-size-bytes", "1")]);
    let sample = TableSample::collect(
        &metadata,
        fixture.metadata_location(),
        fixture.file_io(),
        &StatisticsConfig::default(),
    )
    .await
    .unwrap();
    assert_eq!(sample.target_file_size_bytes, 1);
    assert_eq!(sample.small_files, 0);
}

#[tokio::test]
async fn sampling_never_reads_a_data_file() {
    let fixture = mixed().await;
    for name in ["x0", "x1", "u0"] {
        std::fs::remove_file(fixture_file(&fixture, name)).unwrap();
    }

    let sample = sample(&fixture, &StatisticsConfig::default()).await;
    assert_eq!(sample.data_files, 3);
    assert_eq!(sample.record_count, 6);
}

#[tokio::test]
async fn the_entry_bound_refuses_a_larger_table() {
    let fixture = mixed().await;
    let config = StatisticsConfig {
        max_manifest_entries: 1,
        ..StatisticsConfig::default()
    };

    let error = TableSample::collect(
        fixture.metadata(),
        fixture.metadata_location(),
        fixture.file_io(),
        &config,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, StatisticsError::TooLarge { limit: 1 }));
}

#[tokio::test]
async fn a_table_without_snapshots_is_an_empty_sample() {
    let fixture = TableSpec::new(tagged()).generate().await.unwrap();
    let sample = sample(&fixture, &StatisticsConfig::default()).await;

    assert_eq!(sample.snapshot_id, None);
    assert_eq!(sample.last_operation, None);
    assert_eq!(sample.data_files, 0);
    assert_eq!(sample.size_histogram.len(), 6);
    assert!(sample.size_histogram.iter().all(|bucket| bucket.files == 0));
    assert_eq!(sample.partition_count, 0);
    assert_eq!(sample.snapshot_count, 0);
}

#[tokio::test]
async fn snapshot_history_lists_every_commit_newest_first() {
    let fixture = mixed().await;
    let entries = snapshot_history(fixture.metadata(), 10);

    assert_eq!(entries.len(), 3);
    let ids: Vec<i64> = entries.iter().map(|entry| entry.snapshot_id).collect();
    assert_eq!(ids, [3, 2, 1]);
    let operations: Vec<&str> = entries
        .iter()
        .map(|entry| entry.operation.as_str())
        .collect();
    assert_eq!(operations, ["delete", "append", "append"]);
    assert_eq!(entries[0].parent_snapshot_id, Some(2));
    assert_eq!(entries[2].parent_snapshot_id, None);
    assert!(
        entries
            .windows(2)
            .all(|pair| pair[0].committed_at >= pair[1].committed_at)
    );
    // The fixture declares no summary properties, so the engine's property
    // map is empty rather than synthesized.
    assert!(entries.iter().all(|entry| entry.properties.is_empty()));
}

#[tokio::test]
async fn snapshot_history_is_bounded_to_the_newest_entries() {
    let fixture = mixed().await;
    let entries = snapshot_history(fixture.metadata(), 2);

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].snapshot_id, 3);
    assert_eq!(entries[1].snapshot_id, 2);
}
