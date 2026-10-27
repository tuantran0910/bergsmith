//! Reachability and selection over a real native Iceberg fixture.

#![expect(clippy::unwrap_used, reason = "test helpers fail the test")]

mod support;

use bergsmith_fixtures::{Commit, TableSpec};
use bergsmith_maintenance::orphans::{OrphanPolicy, ReachabilityBounds, Reachable, select_orphans};
use bergsmith_storage::{Location, ObjectStore, StorageProfile};
use chrono::{Duration, Utc};
use iceberg::spec::ManifestList;
use support::{data, tagged};

const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[tokio::test]
async fn every_live_file_is_reachable_and_only_stale_unreachable_objects_are_candidates() {
    let fixture = TableSpec::new(tagged())
        .commit(Commit::new().file(data("x", 2)))
        .generate()
        .await
        .unwrap();
    let table = Location::parse(fixture.metadata().location()).unwrap();
    let bounds = ReachabilityBounds::new(1_000, 100_000, 64 * 1024 * 1024, READ_TIMEOUT).unwrap();

    let reachable = Reachable::capture(
        fixture.metadata(),
        fixture.metadata_location(),
        &[],
        fixture.file_io(),
        bounds,
    )
    .await
    .unwrap();
    assert!(
        reachable.len() >= 4,
        "metadata, list, manifest, and data file"
    );
    assert!(reachable.manifests() >= 1);
    assert!(reachable.entries() >= 1);
    assert!(reachable.contains(&Location::parse(fixture.metadata_location()).unwrap()));

    let store = ObjectStore::open(&StorageProfile::LocalFs, None).unwrap();
    let listed = store.list(&table).await.unwrap();
    assert!(!listed.is_empty());
    for entry in &listed {
        assert!(
            reachable.contains(&entry.location),
            "the fixture lists only live files: {}",
            entry.location
        );
    }

    let policy = OrphanPolicy::new(0, 0, 1_000).unwrap();
    let reachable_decision = select_orphans(
        &reachable,
        &listed,
        fixture.metadata().location(),
        &format!("{}/data", fixture.metadata().location()),
        &policy,
        Utc::now(),
    )
    .unwrap();
    assert!(reachable_decision.is_empty());

    let distant = Utc::now() + Duration::days(400);
    let unreachable_decision = select_orphans(
        &Reachable::default(),
        &listed,
        fixture.metadata().location(),
        &format!("{}/data", fixture.metadata().location()),
        &policy,
        distant,
    )
    .unwrap();
    assert_eq!(unreachable_decision.candidates.len(), listed.len());
    assert_eq!(unreachable_decision.retained_bergsmith, 0);
}

#[tokio::test]
async fn the_scan_bound_fails_instead_of_truncating() {
    let fixture = TableSpec::new(tagged())
        .commit(Commit::new().file(data("x", 2)))
        .commit(Commit::new().file(data("y", 2)))
        .generate()
        .await
        .unwrap();
    assert!(ReachabilityBounds::new(1, 0, 1, READ_TIMEOUT).is_err());
    assert!(ReachabilityBounds::new(1, 1, 1, std::time::Duration::ZERO).is_err());

    let bounds = ReachabilityBounds::new(1, 1_000, 64 * 1024 * 1024, READ_TIMEOUT).unwrap();
    let error = Reachable::capture(
        fixture.metadata(),
        fixture.metadata_location(),
        &[],
        fixture.file_io(),
        bounds,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            error,
            bergsmith_maintenance::orphans::OrphanError::ScanTooLarge { .. }
        ),
        "a scan past its bound must fail instead of deleting from a partial view"
    );
}

#[tokio::test]
async fn manifests_shared_by_snapshots_are_read_and_counted_once() {
    let fixture = TableSpec::new(tagged())
        .commit(Commit::new().file(data("x", 2)))
        .commit(Commit::new().file(data("y", 2)))
        .commit(Commit::new().file(data("z", 2)))
        .generate()
        .await
        .unwrap();
    let metadata = fixture.metadata();
    let mut listed_manifests = 0_usize;
    let mut distinct = std::collections::BTreeMap::new();
    for snapshot in metadata.snapshots() {
        let bytes = fixture
            .file_io()
            .new_input(snapshot.manifest_list())
            .unwrap()
            .read()
            .await
            .unwrap();
        let list = ManifestList::parse_with_version(&bytes, metadata.format_version()).unwrap();
        for manifest in list.entries() {
            listed_manifests += 1;
            let entries = manifest
                .load_manifest(fixture.file_io())
                .await
                .unwrap()
                .entries()
                .len();
            distinct.insert(manifest.manifest_path.clone(), entries);
        }
    }
    assert!(
        listed_manifests > distinct.len(),
        "the fixture must carry manifests forward for this test to mean anything"
    );
    let unique_entries: usize = distinct.values().sum();

    // Bounds equal to the distinct totals pass only if shared manifests are
    // counted once; counting them per snapshot would cross both bounds.
    let bounds = ReachabilityBounds::new(
        distinct.len(),
        unique_entries,
        64 * 1024 * 1024,
        READ_TIMEOUT,
    )
    .unwrap();
    let reachable = Reachable::capture(
        metadata,
        fixture.metadata_location(),
        &[],
        fixture.file_io(),
        bounds,
    )
    .await
    .unwrap();
    assert_eq!(reachable.manifests(), distinct.len());
    assert_eq!(reachable.entries(), unique_entries);
    for path in distinct.keys() {
        assert!(reachable.contains(&Location::parse(path).unwrap()));
    }
}
