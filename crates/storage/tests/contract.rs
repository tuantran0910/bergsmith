//! The storage contract suite (BS-005.T5).
//!
//! Every enabled backend must pass the same tests before it is certified in
//! `docs/support-matrix.md`: memory, the local filesystem, and S3 through
//! RustFS (`BERGSMITH_TEST_S3_ENDPOINT`, started by `make test`). Each test
//! works under a fresh random prefix, so runs never interfere.

// clippy's `allow-expect-in-tests` covers only `#[test]` functions; the shared
// helpers below should also fail the test on any error.
#![expect(clippy::expect_used, reason = "test helpers fail the test")]

use std::collections::BTreeMap;

use bergsmith_storage::{
    CredentialCell, Location, ObjectStore, S3Profile, StorageCredential, StorageError,
    StorageProfile,
};
use bytes::Bytes;
use uuid::Uuid;

/// A backend under test: its store and a fresh root to work under.
struct Fixture {
    store: ObjectStore,
    root: Location,
}

impl Fixture {
    fn at(&self, path: &str) -> Location {
        path.split('/')
            .try_fold(self.root.clone(), |location, segment| {
                location.child(segment)
            })
            .expect("test paths are valid")
    }

    async fn put(&self, path: &str, body: &'static str) {
        self.store
            .write(&self.at(path), Bytes::from_static(body.as_bytes()))
            .await
            .expect("write succeeds");
    }

    /// Paths of every object under `path`, relative to the root.
    async fn listed(&self, path: &str) -> Vec<(String, u64)> {
        let prefix = if path.is_empty() {
            self.root.clone()
        } else {
            self.at(path)
        };
        self.store
            .list(&prefix)
            .await
            .expect("list succeeds")
            .into_iter()
            .map(|entry| {
                let relative = entry
                    .location
                    .key()
                    .strip_prefix(self.root.key())
                    .expect("listed under the root")
                    .trim_start_matches('/')
                    .to_owned();
                (relative, entry.size)
            })
            .collect()
    }
}

fn memory() -> Fixture {
    Fixture {
        store: ObjectStore::open(&StorageProfile::Memory, None).expect("memory store"),
        root: Location::parse(&format!("memory://contract/{}", Uuid::now_v7()))
            .expect("valid root"),
    }
}

fn local_fs() -> Fixture {
    let dir = std::env::temp_dir()
        .canonicalize()
        .expect("temp dir exists")
        .join("bergsmith-contract")
        .join(Uuid::now_v7().to_string());
    std::fs::create_dir_all(&dir).expect("temp dir is writable");
    Fixture {
        store: ObjectStore::open(&StorageProfile::LocalFs, None).expect("fs store"),
        root: Location::parse(&format!("file://{}", dir.display())).expect("valid root"),
    }
}

const S3_BUCKET: &str = "storage-contract";

fn s3_profile() -> StorageProfile {
    StorageProfile::S3(S3Profile {
        endpoint: Some(
            std::env::var("BERGSMITH_TEST_S3_ENDPOINT")
                .unwrap_or_else(|_| "http://127.0.0.1:59000".to_owned()),
        ),
        region: "us-east-1".to_owned(),
        path_style_access: true,
    })
}

/// Development credentials for the Compose RustFS, scoped to `prefix`.
fn s3_credential(prefix: &str, secret: &str) -> StorageCredential {
    let config = BTreeMap::from([
        ("s3.access-key-id".to_owned(), "rustfsadmin".to_owned()),
        ("s3.secret-access-key".to_owned(), secret.to_owned()),
    ]);
    StorageCredential::from_vended(prefix, &config).expect("valid credential")
}

fn s3() -> Fixture {
    let cell = CredentialCell::new(vec![s3_credential(
        &format!("s3://{S3_BUCKET}"),
        "rustfsadmin",
    )]);
    Fixture {
        store: ObjectStore::open(&s3_profile(), Some(cell)).expect("s3 store"),
        root: Location::parse(&format!("s3://{S3_BUCKET}/{}", Uuid::now_v7())).expect("valid root"),
    }
}

async fn objects_round_trip(fixture: Fixture) {
    fixture.put("data/a.parquet", "hello").await;

    let read = fixture
        .store
        .read(&fixture.at("data/a.parquet"))
        .await
        .expect("read succeeds");

    assert_eq!(read, Bytes::from_static(b"hello"));
}

async fn exists_follows_writes_and_deletes(fixture: Fixture) {
    let location = fixture.at("a.parquet");
    assert!(!fixture.store.exists(&location).await.expect("exists"));

    fixture.put("a.parquet", "x").await;
    assert!(fixture.store.exists(&location).await.expect("exists"));

    fixture.store.delete(&location).await.expect("delete");
    assert!(!fixture.store.exists(&location).await.expect("exists"));
}

async fn listing_is_recursive_sorted_and_sized(fixture: Fixture) {
    fixture.put("job/attempt/b.parquet", "bb").await;
    fixture.put("job/attempt/nested/c.parquet", "ccc").await;
    fixture.put("job/a.parquet", "a").await;

    let listed = fixture.listed("job").await;

    assert_eq!(
        listed,
        [
            ("job/a.parquet".to_owned(), 1),
            ("job/attempt/b.parquet".to_owned(), 2),
            ("job/attempt/nested/c.parquet".to_owned(), 3),
        ]
    );
}

async fn listing_compares_whole_segments(fixture: Fixture) {
    fixture.put("job/a.parquet", "a").await;
    fixture.put("job-other/b.parquet", "b").await;

    let listed = fixture.listed("job").await;

    assert_eq!(listed, [("job/a.parquet".to_owned(), 1)]);
}

async fn a_bounded_listing_refuses_more_than_its_limit(fixture: Fixture) {
    for name in ["a", "b", "c"] {
        fixture.put(&format!("job/{name}.parquet"), name).await;
    }
    let prefix = fixture.at("job");

    let at_limit = fixture
        .store
        .list_bounded(&prefix, 3)
        .await
        .expect("a listing at its bound succeeds");
    assert_eq!(at_limit.len(), 3);
    let over = fixture.store.list_bounded(&prefix, 2).await;
    assert!(
        matches!(over, Err(StorageError::ListingTooLarge { limit: 2 })),
        "a listing past its bound fails instead of truncating: {over:?}"
    );
}

async fn listing_an_empty_prefix_returns_nothing(fixture: Fixture) {
    assert!(fixture.listed("missing").await.is_empty());
}

async fn prefix_deletion_removes_only_the_subtree(fixture: Fixture) {
    fixture.put("job/attempt/a.parquet", "a").await;
    fixture.put("job/attempt/nested/b.parquet", "b").await;
    fixture.put("job-other/c.parquet", "c").await;

    fixture
        .store
        .delete_prefix(&fixture.at("job"))
        .await
        .expect("delete prefix");

    assert_eq!(
        fixture.listed("").await,
        [("job-other/c.parquet".to_owned(), 1)]
    );
}

async fn deleting_missing_objects_succeeds(fixture: Fixture) {
    fixture
        .store
        .delete(&fixture.at("missing.parquet"))
        .await
        .expect("delete missing object");
    fixture
        .store
        .delete_prefix(&fixture.at("missing"))
        .await
        .expect("delete missing prefix");
}

async fn reading_a_missing_object_fails(fixture: Fixture) {
    assert!(
        fixture
            .store
            .read(&fixture.at("missing.parquet"))
            .await
            .is_err()
    );
}

macro_rules! contract_suite {
    ($backend:ident) => {
        mod $backend {
            use super::*;

            #[tokio::test]
            async fn objects_round_trip() {
                super::objects_round_trip($backend()).await;
            }

            #[tokio::test]
            async fn exists_follows_writes_and_deletes() {
                super::exists_follows_writes_and_deletes($backend()).await;
            }

            #[tokio::test]
            async fn listing_is_recursive_sorted_and_sized() {
                super::listing_is_recursive_sorted_and_sized($backend()).await;
            }

            #[tokio::test]
            async fn listing_compares_whole_segments() {
                super::listing_compares_whole_segments($backend()).await;
            }

            #[tokio::test]
            async fn a_bounded_listing_refuses_more_than_its_limit() {
                super::a_bounded_listing_refuses_more_than_its_limit($backend()).await;
            }

            #[tokio::test]
            async fn listing_an_empty_prefix_returns_nothing() {
                super::listing_an_empty_prefix_returns_nothing($backend()).await;
            }

            #[tokio::test]
            async fn prefix_deletion_removes_only_the_subtree() {
                super::prefix_deletion_removes_only_the_subtree($backend()).await;
            }

            #[tokio::test]
            async fn deleting_missing_objects_succeeds() {
                super::deleting_missing_objects_succeeds($backend()).await;
            }

            #[tokio::test]
            async fn reading_a_missing_object_fails() {
                super::reading_a_missing_object_fails($backend()).await;
            }
        }
    };
}

contract_suite!(memory);
contract_suite!(local_fs);
contract_suite!(s3);

// S3-specific credential behavior.

#[tokio::test]
async fn s3_refuses_locations_outside_the_credential_scope() {
    let root = format!("s3://{S3_BUCKET}/{}", Uuid::now_v7());
    let cell = CredentialCell::new(vec![s3_credential(
        &format!("{root}/allowed"),
        "rustfsadmin",
    )]);
    let store = ObjectStore::open(&s3_profile(), Some(cell)).expect("s3 store");
    let allowed = Location::parse(&format!("{root}/allowed/a")).expect("valid");
    let denied = Location::parse(&format!("{root}/denied/a")).expect("valid");

    store
        .write(&allowed, Bytes::from_static(b"a"))
        .await
        .expect("write inside the scope");
    let refused = store.write(&denied, Bytes::from_static(b"a")).await;

    assert!(refused.is_err(), "no credential covers {denied}");
}

#[tokio::test]
async fn s3_uses_refreshed_credentials_without_rebuilding_storage() {
    let prefix = format!("s3://{S3_BUCKET}");
    let cell = CredentialCell::new(vec![s3_credential(&prefix, "wrong-secret")]);
    let store = ObjectStore::open(&s3_profile(), Some(cell.clone())).expect("s3 store");
    let location =
        Location::parse(&format!("{prefix}/{}/a", Uuid::now_v7())).expect("valid location");

    let stale = store.write(&location, Bytes::from_static(b"a")).await;
    cell.replace(vec![s3_credential(&prefix, "rustfsadmin")]);
    let refreshed = store.write(&location, Bytes::from_static(b"a")).await;

    assert!(stale.is_err(), "the wrong secret is rejected");
    refreshed.expect("the refreshed secret works");
}
