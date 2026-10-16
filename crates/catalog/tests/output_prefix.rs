//! Catalog acceptance of attempt outputs, and vended-credential scoping and
//! refresh for them, against the Compose Lakekeeper (BS-007.T4).
//!
//! Outputs are written to `<data-location>/bergsmith/<job_id>/<attempt_id>/`
//! with the table's vended credentials, published by a rewrite, and read
//! back with credentials vended afresh, as a reader would.

// clippy's `allow-*-in-tests` settings cover only `#[test]` functions; the
// helpers below fail the test on setup errors.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers fail the test on setup errors"
)]

mod support;

use bergsmith_catalog::{CatalogConnection, CommitOutcome, LoadedTable, TableName};
use bergsmith_common::ids::AttemptId;
use bergsmith_storage::{CredentialCell, Location, ObjectStore};
use bytes::Bytes;
use iceberg::spec::ManifestList;
use serde_json::json;
use support::commit::{
    baseline, data_file_at, rewrite, rustfs_profile, table_with_files, vended_store,
};

/// Writes an output under a fresh attempt prefix of the table's data
/// location, publishes it in place of `removed`, and returns its location.
async fn publish_output(
    connection: &CatalogConnection,
    name: &TableName,
    removed: Vec<iceberg::spec::DataFile>,
) -> (LoadedTable, Location) {
    let table = connection.load_table(name, true).await.unwrap();
    let baseline = baseline(connection, name).await;
    let proposal = rewrite(&baseline, removed, Vec::new());
    let output = table
        .data_location()
        .unwrap()
        .attempt_output_prefix(proposal.job_id, AttemptId::generate())
        .child("part-00000.parquet")
        .unwrap();
    vended_store(&table)
        .write(&output, Bytes::from_static(b"rows"))
        .await
        .unwrap();
    let mut proposal = proposal;
    proposal.added = vec![data_file_at(output.to_string(), 1)];

    let prepared = proposal.prepare(&baseline).await.unwrap();
    let outcome = connection.commit(&prepared.request, None).await.unwrap();

    assert!(
        matches!(outcome, CommitOutcome::Committed(_)),
        "{outcome:?}"
    );
    (table, output)
}

/// Whether the published table lists `output` as a live data file.
async fn is_live(connection: &CatalogConnection, name: &TableName, output: &Location) -> bool {
    let table = connection.load_table(name, true).await.unwrap();
    let snapshot = table.metadata.current_snapshot().unwrap();
    let file_io = vended_store(&table).file_io().clone();
    let list = file_io
        .new_input(snapshot.manifest_list())
        .unwrap()
        .read()
        .await
        .unwrap();
    let manifests =
        ManifestList::parse_with_version(&list, table.metadata.format_version()).unwrap();
    for manifest in manifests.entries() {
        for entry in manifest.load_manifest(&file_io).await.unwrap().entries() {
            if entry.is_alive() && entry.data_file().file_path() == output.to_string() {
                return true;
            }
        }
    }
    false
}

#[tokio::test]
async fn outputs_under_the_attempt_prefix_are_published_and_readable() {
    let (_warehouse, connection, name, files) = table_with_files(&["a"]).await;

    let (table, output) = publish_output(&connection, &name, files).await;

    let data = Location::parse(&format!("{}/data", table.location)).unwrap();
    assert!(output.is_within(&data.child("bergsmith").unwrap()));
    assert!(is_live(&connection, &name, &output).await);
    let reader = connection.load_table(&name, true).await.unwrap();
    assert_eq!(
        vended_store(&reader).read(&output).await.unwrap(),
        Bytes::from_static(b"rows")
    );
}

/// Lakekeeper v0.14.0 manages data locations itself and refuses
/// `write.data.path`, so its attempt outputs always go under
/// `<table-location>/data/bergsmith/`.
#[tokio::test]
async fn lakekeeper_refuses_write_data_path() {
    let (warehouse, connection, name, _) = table_with_files(&["a"]).await;
    let table = connection.load_table(&name, false).await.unwrap();

    let (status, body) = warehouse
        .try_change_metadata(
            &name,
            json!([{"action": "set-properties",
                "updates": {"write.data.path": format!("{}/custom-data", table.location)}}]),
        )
        .await;

    assert!(status.is_client_error(), "{status}: {body}");
    assert!(body.contains("write.data.path"), "{body}");
    assert_eq!(
        connection
            .load_table(&name, false)
            .await
            .unwrap()
            .data_location()
            .unwrap(),
        table.location.child("data").unwrap()
    );
}

#[tokio::test]
async fn refreshed_credentials_keep_writing_attempt_outputs_only() {
    let (_warehouse, connection, name, _) = table_with_files(&["a"]).await;
    let table = connection.load_table(&name, true).await.unwrap();
    let cell = CredentialCell::new(table.credentials.clone());
    let store = ObjectStore::open(&rustfs_profile(), Some(cell.clone())).unwrap();
    let prefix = table.data_location().unwrap().attempt_output_prefix(
        bergsmith_common::ids::JobId::generate(),
        AttemptId::generate(),
    );

    cell.replace(connection.refresh_credentials(&table).await.unwrap());
    let inside = store
        .write(
            &prefix.child("after-refresh").unwrap(),
            Bytes::from_static(b"x"),
        )
        .await;
    let outside = store
        .write(
            &Location::parse(&format!("{}-other/x", table.location)).unwrap(),
            Bytes::from_static(b"x"),
        )
        .await;

    assert!(inside.is_ok(), "{inside:?}");
    assert!(outside.is_err(), "writes outside the table are denied");
}
