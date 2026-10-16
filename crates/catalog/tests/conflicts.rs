//! Conflict fixtures against the Compose Lakekeeper (BS-007.T1).
//!
//! A rewrite is prepared on a baseline, another writer then changes the
//! table, and the rewrite is sent. Each change must be seen by the reload
//! check and rejected by the catalog through the expected original
//! requirement. A change no requirement covers must still be reported by the
//! reload check, even though the catalog accepts the commit.

// clippy's `allow-*-in-tests` settings cover only `#[test]` functions; the
// helpers below fail the test on setup errors.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test helpers fail the test on setup errors"
)]

mod support;

use bergsmith_catalog::{
    BaselineChange, CatalogConnection, CommitOutcome, PreparedCommit, RejectionKind,
    SnapshotEvidence, TableName,
};
use iceberg::TableRequirement;
use iceberg::spec::TableMetadata;
use serde_json::Value;
use support::Warehouse;
use support::commit::{
    add_column, add_partition_spec, add_sort_order, append, baseline, data_file, rewrite,
    set_owner, table_with_files,
};

/// Another writer's change to the table.
enum Change {
    Append,
    DeleteFile,
    Metadata(Value),
}

/// The names of the requirements `metadata` no longer satisfies.
fn failing(requirements: &[TableRequirement], metadata: &TableMetadata) -> Vec<&'static str> {
    requirements
        .iter()
        .filter(|requirement| requirement.check(Some(metadata)).is_err())
        .map(|requirement| match requirement {
            TableRequirement::NotExist => "assert-create",
            TableRequirement::UuidMatch { .. } => "assert-table-uuid",
            TableRequirement::RefSnapshotIdMatch { .. } => "assert-ref-snapshot-id",
            TableRequirement::LastAssignedFieldIdMatch { .. } => "assert-last-assigned-field-id",
            TableRequirement::CurrentSchemaIdMatch { .. } => "assert-current-schema-id",
            TableRequirement::LastAssignedPartitionIdMatch { .. } => {
                "assert-last-assigned-partition-id"
            }
            TableRequirement::DefaultSpecIdMatch { .. } => "assert-default-spec-id",
            TableRequirement::DefaultSortOrderIdMatch { .. } => "assert-default-sort-order-id",
        })
        .collect()
}

struct Scenario {
    connection: CatalogConnection,
    name: TableName,
    prepared: PreparedCommit,
    changes: Vec<BaselineChange>,
    failing: Vec<&'static str>,
}

/// Prepares a rewrite of `a` on a baseline, applies `change`, and reports
/// what the reload check and the original requirements see.
async fn scenario(change: Change) -> Scenario {
    let (warehouse, connection, name, files) = table_with_files(&["a", "b"]).await;
    let stale = baseline(&connection, &name).await;
    let prepared = rewrite(
        &stale,
        vec![files[0].clone()],
        vec![data_file(&stale, "c", 1)],
    )
    .prepare(&stale)
    .await
    .unwrap();
    apply(&warehouse, &connection, &name, change, files[1].clone()).await;
    let reloaded = connection.load_table(&name, false).await.unwrap();
    let requirements = prepared.request.decode().unwrap().requirements;
    Scenario {
        changes: stale.changes(&reloaded),
        failing: failing(&requirements, &reloaded.metadata),
        connection,
        name,
        prepared,
    }
}

async fn apply(
    warehouse: &Warehouse,
    connection: &CatalogConnection,
    name: &TableName,
    change: Change,
    other_file: iceberg::spec::DataFile,
) {
    match change {
        Change::Append => {
            append(connection, name, &["late"]).await;
        }
        Change::DeleteFile => {
            let current = baseline(connection, name).await;
            let delete = rewrite(&current, vec![other_file], Vec::new())
                .prepare(&current)
                .await
                .unwrap();
            support::commit::commit(connection, &delete.request).await;
        }
        Change::Metadata(updates) => warehouse.change_metadata(name, updates).await,
    }
}

impl Scenario {
    /// Sends the stale rewrite and requires a requirement rejection that
    /// left no snapshot behind.
    async fn assert_rejected(&self) {
        let outcome = self
            .connection
            .commit(&self.prepared.request, None)
            .await
            .unwrap();
        let CommitOutcome::Rejected(rejection) = outcome else {
            panic!("the stale rewrite is rejected: {outcome:?}");
        };
        assert_eq!(rejection.kind, RejectionKind::RequirementFailed);
        assert_eq!(
            self.connection
                .snapshot_evidence(&self.name, &self.prepared.expected)
                .await
                .unwrap(),
            SnapshotEvidence::Absent
        );
    }

    fn main_moved(&self) -> bool {
        self.changes
            .iter()
            .any(|change| matches!(change, BaselineChange::MainSnapshot { .. }))
    }
}

#[tokio::test]
async fn a_concurrent_append_is_rejected() {
    let scenario = scenario(Change::Append).await;

    assert_eq!(scenario.failing, ["assert-ref-snapshot-id"]);
    assert!(scenario.main_moved());
    scenario.assert_rejected().await;
}

#[tokio::test]
async fn a_concurrent_delete_is_rejected() {
    let scenario = scenario(Change::DeleteFile).await;

    assert_eq!(scenario.failing, ["assert-ref-snapshot-id"]);
    assert!(scenario.main_moved());
    scenario.assert_rejected().await;
}

#[tokio::test]
async fn a_concurrent_schema_change_is_rejected() {
    let scenario = scenario(Change::Metadata(add_column())).await;

    assert_eq!(
        scenario.failing,
        ["assert-current-schema-id", "assert-last-assigned-field-id"]
    );
    assert_eq!(
        scenario.changes,
        [
            BaselineChange::MetadataLocation,
            BaselineChange::CurrentSchema,
            BaselineChange::LastColumnId
        ]
    );
    scenario.assert_rejected().await;
}

#[tokio::test]
async fn a_concurrent_partition_spec_change_is_rejected() {
    let scenario = scenario(Change::Metadata(add_partition_spec())).await;

    assert_eq!(
        scenario.failing,
        [
            "assert-default-spec-id",
            "assert-last-assigned-partition-id"
        ]
    );
    assert_eq!(
        scenario.changes,
        [
            BaselineChange::MetadataLocation,
            BaselineChange::DefaultSpec,
            BaselineChange::LastPartitionId
        ]
    );
    scenario.assert_rejected().await;
}

#[tokio::test]
async fn a_concurrent_sort_order_change_is_rejected() {
    let scenario = scenario(Change::Metadata(add_sort_order())).await;

    assert_eq!(scenario.failing, ["assert-default-sort-order-id"]);
    assert_eq!(
        scenario.changes,
        [
            BaselineChange::MetadataLocation,
            BaselineChange::DefaultSortOrder
        ]
    );
    scenario.assert_rejected().await;
}

/// Standard REST cannot assert properties or the metadata reference, so a
/// benign property change coexists with a valid commit. The reload check
/// still reports it, which is why publication rechecks the baseline first.
#[tokio::test]
async fn an_unasserted_metadata_change_is_seen_only_by_the_reload_check() {
    let scenario = scenario(Change::Metadata(set_owner())).await;

    assert!(scenario.failing.is_empty(), "{:?}", scenario.failing);
    assert_eq!(scenario.changes, [BaselineChange::MetadataLocation]);
    let outcome = scenario
        .connection
        .commit(&scenario.prepared.request, None)
        .await
        .unwrap();
    assert!(
        matches!(outcome, CommitOutcome::Committed(_)),
        "{outcome:?}"
    );
}
