//! Commit fixtures: baselines with vended credentials, data file records,
//! appends, rewrites, and raw metadata changes made by "another writer".

use bergsmith_catalog::{
    Baseline, CatalogConnection, CommitOutcome, CommitRequest, LoadedTable, RewriteProposal,
    TableName, new_snapshot_id,
};
use bergsmith_common::ids::{JobId, PublicationId};
use bergsmith_storage::{CredentialCell, ObjectStore, S3Profile, StorageProfile};
use iceberg::spec::{DataContentType, DataFile, DataFileBuilder, DataFileFormat, Struct};
use iceberg::transaction::ApplyTransactionAction;
use serde_json::{Value, json};

use super::{Warehouse, s3_endpoint, server};

/// The Compose RustFS as a storage profile.
pub(crate) fn rustfs_profile() -> StorageProfile {
    StorageProfile::S3(S3Profile {
        endpoint: Some(s3_endpoint()),
        region: "us-east-1".to_owned(),
        path_style_access: true,
    })
}

/// A store that uses `table`'s vended credentials.
pub(crate) fn vended_store(table: &LoadedTable) -> ObjectStore {
    ObjectStore::open(
        &rustfs_profile(),
        Some(CredentialCell::new(table.credentials.clone())),
    )
    .expect("the store opens")
}

/// The table as it is now, pinned as a baseline whose `FileIO` uses the
/// table's vended credentials.
pub(crate) async fn baseline(connection: &CatalogConnection, name: &TableName) -> Baseline {
    let loaded = connection
        .load_table(name, true)
        .await
        .expect("the table loads");
    Baseline::new(&loaded, vended_store(&loaded).file_io().clone()).expect("the baseline builds")
}

/// A data file record at `path`; the commit adapter never reads data files.
pub(crate) fn data_file_at(path: String, records: u64) -> DataFile {
    DataFileBuilder::default()
        .content(DataContentType::Data)
        .file_path(path)
        .file_format(DataFileFormat::Parquet)
        .file_size_in_bytes(records * 10)
        .record_count(records)
        .partition_spec_id(0)
        .partition(Struct::empty())
        .build()
        .expect("the data file is valid")
}

/// A data file record named `name` under the table's `data/` directory.
pub(crate) fn data_file(baseline: &Baseline, name: &str, records: u64) -> DataFile {
    data_file_at(
        format!("{}/data/{name}.parquet", baseline.metadata().location()),
        records,
    )
}

/// Sends `request` and requires it to commit.
pub(crate) async fn commit(connection: &CatalogConnection, request: &CommitRequest) {
    let outcome = connection
        .commit(request, None)
        .await
        .expect("the request is sent");
    assert!(
        matches!(outcome, CommitOutcome::Committed(_)),
        "{outcome:?}"
    );
}

/// Appends files named `names` and returns them.
pub(crate) async fn append(
    connection: &CatalogConnection,
    name: &TableName,
    names: &[&str],
) -> Vec<DataFile> {
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
        .expect("the append builds");
    commit(connection, &request).await;
    files
}

/// A rewrite proposal with fresh IDs.
pub(crate) fn rewrite(
    baseline: &Baseline,
    removed: Vec<DataFile>,
    added: Vec<DataFile>,
) -> RewriteProposal {
    RewriteProposal {
        job_id: JobId::generate(),
        publication_id: PublicationId::generate(),
        snapshot_id: new_snapshot_id(baseline.metadata()),
        removed,
        added,
    }
}

/// A fresh warehouse with `db.events` holding one append of `names`.
pub(crate) async fn table_with_files(
    names: &[&str],
) -> (Warehouse, CatalogConnection, TableName, Vec<DataFile>) {
    let warehouse = Warehouse::create().await;
    let name = warehouse.create_table("db", "events").await;
    let connection = warehouse.connect().await;
    let files = append(&connection, &name, names).await;
    (warehouse, connection, name, files)
}

impl Warehouse {
    /// Applies `updates` to `table` with no requirements, as an unrelated
    /// writer would.
    pub(crate) async fn change_metadata(&self, table: &TableName, updates: Value) {
        let (status, body) = self.try_change_metadata(table, updates).await;
        assert!(status.is_success(), "metadata change failed: {body}");
    }

    /// Like [`change_metadata`](Self::change_metadata), returning the
    /// status and body instead of requiring success.
    pub(crate) async fn try_change_metadata(
        &self,
        table: &TableName,
        updates: Value,
    ) -> (reqwest::StatusCode, String) {
        let url = server()
            .join(&format!(
                "catalog/v1/{}/namespaces/{}/tables/{}",
                self.prefix().await,
                table.namespace.join("\u{1f}"),
                table.name
            ))
            .expect("valid URL");
        let response = reqwest::Client::new()
            .post(url)
            .json(&json!({ "requirements": [], "updates": updates }))
            .send()
            .await
            .expect("Lakekeeper is reachable");
        (response.status(), response.text().await.unwrap_or_default())
    }
}

/// Adds a nullable column and makes it current.
pub(crate) fn add_column() -> Value {
    json!([
        {"action": "add-schema", "schema": {"type": "struct", "schema-id": 1, "fields": [
            {"id": 1, "name": "id", "required": true, "type": "long"},
            {"id": 2, "name": "extra", "required": false, "type": "string"}]}},
        {"action": "set-current-schema", "schema-id": -1}
    ])
}

/// Adds a bucket partition spec and makes it the default.
pub(crate) fn add_partition_spec() -> Value {
    json!([
        {"action": "add-spec", "spec": {"fields": [
            {"source-id": 1, "field-id": 1000, "name": "id_bucket", "transform": "bucket[4]"}]}},
        {"action": "set-default-spec", "spec-id": -1}
    ])
}

/// Adds a sort order and makes it the default.
pub(crate) fn add_sort_order() -> Value {
    json!([
        {"action": "add-sort-order", "sort-order": {"order-id": 1, "fields": [
            {"source-id": 1, "transform": "identity", "direction": "asc",
             "null-order": "nulls-first"}]}},
        {"action": "set-default-sort-order", "sort-order-id": -1}
    ])
}

/// Sets a property no plan depends on.
pub(crate) fn set_owner() -> Value {
    json!([{"action": "set-properties", "updates": {"owner": "someone"}}])
}
