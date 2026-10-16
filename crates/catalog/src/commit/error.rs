//! Errors from building a commit. Nothing has been sent when one occurs.

use iceberg::TableRequirement;

/// A commit could not be built. Nothing was sent to the catalog.
#[derive(Debug, thiserror::Error)]
pub enum ProposalError {
    /// iceberg-rust could not build the table or apply an action, for
    /// example because a removed file is not live in the baseline.
    #[error("iceberg could not build the commit")]
    Iceberg(#[source] iceberg::Error),
    /// The actions changed nothing.
    #[error("the commit has no changes")]
    NoChanges,
    /// A rewrite needs a `main` snapshot to replace files in.
    #[error("the baseline has no main snapshot to rewrite")]
    NoMainSnapshot,
    /// A rewrite must remove at least one file.
    #[error("a rewrite must remove at least one file")]
    NothingRemoved,
    /// The proposed snapshot ID is already in the baseline.
    #[error("snapshot ID {0} already exists in the baseline")]
    SnapshotIdInUse(i64),
    /// An action asserted a value that disagrees with the baseline, so it
    /// did not run on the baseline.
    #[error("an action requirement disagrees with the baseline")]
    RequirementConflict {
        /// The baseline's requirement.
        baseline: Box<TableRequirement>,
        /// The action's requirement for the same property.
        action: Box<TableRequirement>,
    },
    /// The built commit does not add the proposed snapshot on top of the
    /// baseline `main` snapshot.
    #[error("the built commit does not add snapshot {snapshot_id} on the baseline")]
    UnexpectedSnapshot {
        /// The proposed snapshot ID.
        snapshot_id: i64,
    },
    /// The request could not be serialized.
    #[error("the commit request could not be serialized")]
    Encode(#[source] serde_json::Error),
}
