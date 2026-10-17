//! Detecting baseline drift before a commit is built (BS-007.T1).
//!
//! Immediately before publication the coordinator reloads the table. Any
//! observed change to the planning baseline means the plan must be redone
//! (`ReplanRequired`). Standard REST requirements cannot assert the metadata
//! reference, format version, or properties, so this check catches changes
//! that the requirements would let through. It does not close the race
//! between the reload and the commit.

use iceberg::spec::{MAIN_BRANCH, TableMetadata};

use crate::commit::baseline::Baseline;
use crate::connection::LoadedTable;

pub use bergsmith_common::table::PLAN_RELEVANT_PROPERTIES;

/// One difference between the baseline and the reloaded table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaselineChange {
    /// The name now refers to a different table.
    TableUuid,
    /// The current metadata file changed: something committed.
    MetadataLocation,
    /// The `main` snapshot moved.
    MainSnapshot {
        /// The baseline `main` snapshot.
        from: Option<i64>,
        /// The current `main` snapshot.
        to: Option<i64>,
    },
    /// The format version changed.
    FormatVersion,
    /// The current schema changed.
    CurrentSchema,
    /// The last assigned field ID changed.
    LastColumnId,
    /// The default partition spec changed.
    DefaultSpec,
    /// The last assigned partition field ID changed.
    LastPartitionId,
    /// The default sort order changed.
    DefaultSortOrder,
    /// A property in [`PLAN_RELEVANT_PROPERTIES`] changed.
    Property(&'static str),
}

impl Baseline {
    /// Every observed difference between the baseline and `current`, in a
    /// fixed order. Empty means no change was observed.
    #[must_use]
    pub fn changes(&self, current: &LoadedTable) -> Vec<BaselineChange> {
        let (old, new) = (self.metadata(), &current.metadata);
        let mut changes = Vec::new();
        if old.uuid() != new.uuid() {
            changes.push(BaselineChange::TableUuid);
        }
        if self.metadata_location() != current.metadata_location.as_deref() {
            changes.push(BaselineChange::MetadataLocation);
        }
        let (from, to) = (main(old), main(new));
        if from != to {
            changes.push(BaselineChange::MainSnapshot { from, to });
        }
        for (changed, change) in [
            (
                old.format_version() != new.format_version(),
                BaselineChange::FormatVersion,
            ),
            (
                old.current_schema_id() != new.current_schema_id(),
                BaselineChange::CurrentSchema,
            ),
            (
                old.last_column_id() != new.last_column_id(),
                BaselineChange::LastColumnId,
            ),
            (
                old.default_partition_spec_id() != new.default_partition_spec_id(),
                BaselineChange::DefaultSpec,
            ),
            (
                old.last_partition_id() != new.last_partition_id(),
                BaselineChange::LastPartitionId,
            ),
            (
                old.default_sort_order_id() != new.default_sort_order_id(),
                BaselineChange::DefaultSortOrder,
            ),
        ] {
            if changed {
                changes.push(change);
            }
        }
        for key in PLAN_RELEVANT_PROPERTIES {
            if old.properties().get(key) != new.properties().get(key) {
                changes.push(BaselineChange::Property(key));
            }
        }
        changes
    }
}

fn main(metadata: &TableMetadata) -> Option<i64> {
    metadata
        .snapshot_for_ref(MAIN_BRANCH)
        .map(|snapshot| snapshot.snapshot_id())
}
