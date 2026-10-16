//! Conditional commits against the Compose Lakekeeper, with manifests
//! written to RustFS through vended credentials (BS-006).

// clippy's `allow-*-in-tests` settings cover only `#[test]` functions; the
// helpers below fail the test on setup errors.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers fail the test on setup errors"
)]

mod support;

use bergsmith_catalog::{
    Baseline, CatalogConnection, CommitOutcome, CommitRequest, IdempotencyKey, RejectionKind,
    RewriteProposal, SnapshotEvidence, TableName, new_snapshot_id,
};
use bergsmith_common::ids::{JobId, PublicationId};
use bergsmith_storage::{CredentialCell, ObjectStore, S3Profile, StorageProfile};
use iceberg::spec::{DataContentType, DataFile, DataFileBuilder, DataFileFormat, Struct};
use iceberg::transaction::ApplyTransactionAction;
use support::{Warehouse, s3_endpoint};

fn rustfs_profile() -> StorageProfile {
    StorageProfile::S3(S3Profile {
        endpoint: Some(s3_endpoint()),
        region: "us-east-1".to_owned(),
        path_style_access: true,
    })
}

/// The table as it is now, pinned as a baseline whose `FileIO` uses the
/// table's vended credentials.
async fn baseline(connection: &CatalogConnection, name: &TableName) -> Baseline {
    let loaded = connection.load_table(name, true).await.unwrap();
    let store = ObjectStore::open(
        &rustfs_profile(),
        Some(CredentialCell::new(loaded.credentials.clone())),
    )
    .unwrap();
    Baseline::new(&loaded, store.file_io().clone()).unwrap()
}

/// A data file record; the commit adapter never reads data files.
fn data_file(baseline: &Baseline, name: &str, records: u64) -> DataFile {
    DataFileBuilder::default()
        .content(DataContentType::Data)
        .file_path(format!(
            "{}/data/{name}.parquet",
            baseline.metadata().location()
        ))
        .file_format(DataFileFormat::Parquet)
        .file_size_in_bytes(records * 10)
        .record_count(records)
        .partition_spec_id(0)
        .partition(Struct::empty())
        .build()
        .unwrap()
}

async fn commit(connection: &CatalogConnection, request: &CommitRequest) {
    let outcome = connection.commit(request, None).await.unwrap();
    assert!(
        matches!(outcome, CommitOutcome::Committed(_)),
        "{outcome:?}"
    );
}

/// Appends files named `names` and returns them.
async fn append(connection: &CatalogConnection, name: &TableName, names: &[&str]) -> Vec<DataFile> {
    let baseline = baseline(connection, name).await;
    let files: Vec<DataFile> = names
        .iter()
        .map(|file| data_file(&baseline, file, 1))
        .collect();
    let request = baseline
        .prepare_with(|transaction| {
            transaction
                .fast_append()
                .add_data_files(files.clone())
                .apply(transaction)
        })
        .await
        .unwrap();
    commit(connection, &request).await;
    files
}

fn rewrite(baseline: &Baseline, removed: Vec<DataFile>, added: Vec<DataFile>) -> RewriteProposal {
    RewriteProposal {
        job_id: JobId::generate(),
        publication_id: PublicationId::generate(),
        snapshot_id: new_snapshot_id(baseline.metadata()),
        removed,
        added,
    }
}

async fn table_with_files(names: &[&str]) -> (CatalogConnection, TableName, Vec<DataFile>) {
    let warehouse = Warehouse::create().await;
    let name = warehouse.create_table("db", "events").await;
    let connection = warehouse.connect().await;
    let files = append(&connection, &name, names).await;
    (connection, name, files)
}

#[tokio::test]
async fn a_rewrite_commits_and_leaves_evidence() {
    let (connection, name, files) = table_with_files(&["a", "b"]).await;
    let baseline = baseline(&connection, &name).await;
    let output = data_file(&baseline, "c", 2);
    let prepared = rewrite(&baseline, files, vec![output])
        .prepare(&baseline)
        .await
        .unwrap();

    let outcome = connection.commit(&prepared.request, None).await.unwrap();

    let CommitOutcome::Committed(committed) = outcome else {
        panic!("the rewrite commits: {outcome:?}");
    };
    let published = SnapshotEvidence::Published {
        sequence_number: baseline.main_snapshot().unwrap().sequence_number() + 1,
        main_head: true,
    };
    assert_eq!(
        prepared
            .expected
            .find_in(committed.metadata.as_ref().unwrap()),
        published
    );
    assert_eq!(
        connection
            .snapshot_evidence(&name, &prepared.expected)
            .await
            .unwrap(),
        published
    );
}

#[tokio::test]
async fn a_proposal_on_a_stale_baseline_is_rejected() {
    let (connection, name, files) = table_with_files(&["a"]).await;
    let stale = baseline(&connection, &name).await;
    let prepared = rewrite(&stale, files, vec![data_file(&stale, "c", 1)])
        .prepare(&stale)
        .await
        .unwrap();
    append(&connection, &name, &["late"]).await;

    let outcome = connection.commit(&prepared.request, None).await.unwrap();

    let CommitOutcome::Rejected(rejection) = outcome else {
        panic!("the stale commit is rejected: {outcome:?}");
    };
    assert_eq!(rejection.kind, RejectionKind::RequirementFailed);
    assert_eq!(
        connection
            .snapshot_evidence(&name, &prepared.expected)
            .await
            .unwrap(),
        SnapshotEvidence::Absent
    );
}

#[tokio::test]
async fn evidence_is_found_after_the_head_advances() {
    let (connection, name, files) = table_with_files(&["a"]).await;
    let baseline = baseline(&connection, &name).await;
    let prepared = rewrite(&baseline, files, vec![data_file(&baseline, "c", 1)])
        .prepare(&baseline)
        .await
        .unwrap();
    commit(&connection, &prepared.request).await;
    append(&connection, &name, &["late"]).await;

    let evidence = connection
        .snapshot_evidence(&name, &prepared.expected)
        .await
        .unwrap();

    assert!(
        matches!(
            evidence,
            SnapshotEvidence::Published {
                main_head: false,
                ..
            }
        ),
        "{evidence:?}"
    );
}

#[tokio::test]
async fn a_replay_with_the_same_key_does_not_publish_twice() {
    let (connection, name, files) = table_with_files(&["a"]).await;
    let baseline = baseline(&connection, &name).await;
    let prepared = rewrite(&baseline, files, vec![data_file(&baseline, "c", 1)])
        .prepare(&baseline)
        .await
        .unwrap();
    let key = IdempotencyKey::generate();
    let persisted = CommitRequest::from_persisted(name.clone(), prepared.request.body().to_vec());

    let first = connection
        .commit(&prepared.request, Some(key))
        .await
        .unwrap();
    let replay = connection.commit(&persisted, Some(key)).await.unwrap();

    assert!(matches!(first, CommitOutcome::Committed(_)), "{first:?}");
    assert!(matches!(replay, CommitOutcome::Committed(_)), "{replay:?}");
    assert_eq!(persisted.sha256(), prepared.request.sha256());
    let after = connection.load_table(&name, false).await.unwrap().metadata;
    assert_eq!(after.snapshots().len(), 2);
    assert_eq!(
        prepared.expected.find_in(&after),
        SnapshotEvidence::Published {
            sequence_number: 2,
            main_head: true
        }
    );
}
