//! Native-file coverage for storage accounting; data contents are never read.
#![expect(clippy::unwrap_used, reason = "test setup and assertions")]
mod support;
use bergsmith_fixtures::{Commit, FileSpec, TableSpec};
use bergsmith_maintenance::{
    orphans::{OrphanError, ReachabilityBounds, Reachable},
    storage_breakdown::{StorageBreakdown, StorageScan},
};
use bergsmith_storage::{Location, ObjectStore, StorageProfile};
use chrono::{Duration, Utc};
use std::sync::Arc;
use support::{data, tagged};
fn bounds() -> ReachabilityBounds {
    ReachabilityBounds::new(
        1000,
        10000,
        64 * 1024 * 1024,
        std::time::Duration::from_secs(30),
    )
    .unwrap()
    .with_scan_limits(100, 128 * 1024 * 1024)
    .unwrap()
}
async fn collect(
    metadata: &iceberg::spec::TableMetadata,
    location: &str,
    unresolved: &[String],
    io: &iceberg::io::FileIO,
    bounds: ReachabilityBounds,
    listed: &[bergsmith_storage::ObjectEntry],
    cutoff: chrono::DateTime<Utc>,
) -> Result<StorageBreakdown, OrphanError> {
    StorageScan::new(bounds, Arc::new(tokio::sync::Semaphore::new(1)))
        .collect(metadata, location, unresolved, io, listed, cutoff)
        .await
}
#[tokio::test]
async fn shared_native_files_are_counted_once_and_rollback_history_stays_retained() {
    let fixture = TableSpec::new(tagged())
        .commit(Commit::new().file(data("x", 2)))
        .commit(
            Commit::new()
                .file(data("y", 2))
                .file(FileSpec::position_deletes("p", [("x", 0)])),
        )
        .generate()
        .await
        .unwrap();
    let store = ObjectStore::open(&StorageProfile::LocalFs, None).unwrap();
    let root = Location::parse(fixture.metadata().location()).unwrap();
    let listed = store.list(&root).await.unwrap();
    let sample = collect(
        fixture.metadata(),
        fixture.metadata_location(),
        &[],
        fixture.file_io(),
        bounds(),
        &listed,
        Utc::now() + Duration::days(10),
    )
    .await
    .unwrap();
    assert_eq!(sample.live.files, 3);
    assert_eq!(sample.retained.files, 0);
    assert_eq!(sample.orphan.files, 0);
    assert_eq!(
        sample.total.bytes,
        sample.live.bytes + sample.metadata.bytes
    );
    let mut json = serde_json::to_value(fixture.metadata()).unwrap();
    json["current-snapshot-id"] = serde_json::json!(1);
    json["refs"]["main"]["snapshot-id"] = serde_json::json!(1);
    json["refs"]["other"] = serde_json::json!({"type":"branch","snapshot-id":2});
    let metadata = serde_json::from_value(json).unwrap();
    let sample = collect(
        &metadata,
        fixture.metadata_location(),
        &[],
        fixture.file_io(),
        bounds(),
        &listed,
        Utc::now(),
    )
    .await
    .unwrap();
    assert_eq!(sample.live.files, 1);
    assert_eq!(sample.retained.files, 2);
    assert_eq!(
        sample.total.bytes,
        sample.live.bytes + sample.retained.bytes + sample.metadata.bytes
    );
    assert!(matches!(
        collect(
            &metadata,
            fixture.metadata_location(),
            &[],
            fixture.file_io(),
            bounds().with_scan_limits(1, 128 * 1024 * 1024).unwrap(),
            &listed,
            Utc::now()
        )
        .await,
        Err(OrphanError::ScanListsTooLarge { limit: 1 })
    ));
    assert!(matches!(
        collect(
            &metadata,
            fixture.metadata_location(),
            &[],
            fixture.file_io(),
            bounds().with_scan_limits(100, 1).unwrap(),
            &listed,
            Utc::now()
        )
        .await,
        Err(OrphanError::ScanBytesTooLarge { limit: 1 })
    ));
    // Every individual Avro object fits, but their combined reads do not.
    let avro_sizes: Vec<u64> = listed
        .iter()
        .filter(|entry| {
            std::path::Path::new(entry.location.key())
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("avro"))
        })
        .map(|entry| entry.size)
        .collect();
    let largest = *avro_sizes.iter().max().unwrap();
    assert!(avro_sizes.iter().sum::<u64>() > largest);
    let cumulative =
        ReachabilityBounds::new(1000, 10_000, largest, std::time::Duration::from_secs(30))
            .unwrap()
            .with_scan_limits(100, largest)
            .unwrap();
    assert!(
        matches!(collect(&metadata, fixture.metadata_location(), &[], fixture.file_io(), cumulative, &listed, Utc::now()).await,
        Err(OrphanError::ScanBytesTooLarge { limit }) if limit == largest)
    );
}
#[tokio::test]
async fn an_empty_table_has_metadata_and_no_invented_live_objects() {
    let fixture = TableSpec::new(tagged()).generate().await.unwrap();
    let store = ObjectStore::open(&StorageProfile::LocalFs, None).unwrap();
    let root = Location::parse(fixture.metadata().location()).unwrap();
    let listed = store.list(&root).await.unwrap();
    let sample = collect(
        fixture.metadata(),
        fixture.metadata_location(),
        &[],
        fixture.file_io(),
        bounds(),
        &listed,
        Utc::now(),
    )
    .await
    .unwrap();
    assert_eq!(sample.live.files, 0);
    assert_eq!(sample.total, sample.metadata);
}

#[tokio::test]
async fn cleanup_capture_does_not_wait_on_reporting_decoder_capacity() {
    let fixture = TableSpec::new(tagged())
        .commit(Commit::new().file(data("x", 2)))
        .generate()
        .await
        .unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(1));
    let held = gate.acquire().await.unwrap();
    let reporting = StorageScan::new(bounds(), gate.clone());
    // A held reporting permit cannot delay the production cleanup entry point.
    let cleanup = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        Reachable::capture(
            fixture.metadata(),
            fixture.metadata_location(),
            &[],
            fixture.file_io(),
            bounds(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!cleanup.is_empty());
    drop(held);
    let store = ObjectStore::open(&StorageProfile::LocalFs, None).unwrap();
    let listed = store
        .list(&Location::parse(fixture.metadata().location()).unwrap())
        .await
        .unwrap();
    assert!(
        reporting
            .collect(
                fixture.metadata(),
                fixture.metadata_location(),
                &[],
                fixture.file_io(),
                &listed,
                Utc::now()
            )
            .await
            .is_ok()
    );
}
