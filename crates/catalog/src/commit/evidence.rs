//! Snapshot evidence: recognizing a proposed snapshot in table metadata
//! after the commit response was lost (BS-006.T5).
//!
//! Evidence is read-only. A snapshot that matches every expected field
//! proves the commit was published, even if it is no longer the `main`
//! head. Absence proves nothing: the commit may still be in flight, or its
//! snapshot may have expired.

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{JobId, PublicationId};
use iceberg::spec::{MAIN_BRANCH, TableMetadata};
use uuid::Uuid;

use crate::commit::proposal::{JOB_ID_PROPERTY, PROPOSAL_HASH_PROPERTY, PUBLICATION_ID_PROPERTY};
use crate::connection::{CatalogConnection, TableName};
use crate::error::CatalogError;

/// What a published proposal looks like, persisted with the commit intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedSnapshot {
    /// The table's `table-uuid`.
    pub table_uuid: Uuid,
    /// The proposed snapshot ID.
    pub snapshot_id: i64,
    /// The baseline `main` snapshot it replaces.
    pub parent_snapshot_id: Option<i64>,
    /// The manifest list the commit wrote.
    pub manifest_list: String,
    /// The publishing job.
    pub job_id: JobId,
    /// The job's publication.
    pub publication_id: PublicationId,
    /// The proposal hash.
    pub proposal_hash: Sha256Digest,
}

/// What table metadata shows about an expected snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotEvidence {
    /// The snapshot is in the table and matches every expected field, so
    /// the commit was published.
    Published {
        /// The sequence number the catalog assigned.
        sequence_number: i64,
        /// Whether the snapshot is still the `main` head.
        main_head: bool,
    },
    /// The table has no snapshot with the expected ID. This never proves
    /// that the commit was not published.
    Absent,
    /// A metadata-only commit's expected post-state holds: the head is the
    /// baseline head, every removed snapshot is absent, and every protected
    /// snapshot is present.
    ExpirationApplied,
    /// A metadata-only commit's expected post-state is not visible yet.
    /// This never proves the commit was not published.
    ExpirationPending,
    /// Something other than the proposal is there. This is an integrity
    /// problem for an operator, never proof of either outcome.
    Mismatch(EvidenceMismatch),
}

/// The first field that differs from the expected snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvidenceMismatch {
    /// The table under this name is a different table.
    TableUuid(Uuid),
    /// A snapshot with the expected ID has another parent.
    ParentSnapshot(Option<i64>),
    /// A snapshot with the expected ID has another manifest list.
    ManifestList(String),
    /// A snapshot with the expected ID has another summary property.
    SummaryProperty {
        /// The property, such as `bergsmith.publication-id`.
        key: &'static str,
        /// The value found, if any.
        found: Option<String>,
    },
}

impl ExpectedSnapshot {
    /// Looks for the expected snapshot in `metadata`.
    #[must_use]
    pub fn find_in(&self, metadata: &TableMetadata) -> SnapshotEvidence {
        if metadata.uuid() != self.table_uuid {
            return SnapshotEvidence::Mismatch(EvidenceMismatch::TableUuid(metadata.uuid()));
        }
        let Some(snapshot) = metadata.snapshot_by_id(self.snapshot_id) else {
            return SnapshotEvidence::Absent;
        };
        if snapshot.parent_snapshot_id() != self.parent_snapshot_id {
            return SnapshotEvidence::Mismatch(EvidenceMismatch::ParentSnapshot(
                snapshot.parent_snapshot_id(),
            ));
        }
        if snapshot.manifest_list() != self.manifest_list {
            return SnapshotEvidence::Mismatch(EvidenceMismatch::ManifestList(
                snapshot.manifest_list().to_owned(),
            ));
        }
        let properties = &snapshot.summary().additional_properties;
        for (key, expected) in [
            (JOB_ID_PROPERTY, self.job_id.to_string()),
            (PUBLICATION_ID_PROPERTY, self.publication_id.to_string()),
            (PROPOSAL_HASH_PROPERTY, self.proposal_hash.to_hex()),
        ] {
            let found = properties.get(key);
            if found != Some(&expected) {
                return SnapshotEvidence::Mismatch(EvidenceMismatch::SummaryProperty {
                    key,
                    found: found.cloned(),
                });
            }
        }
        SnapshotEvidence::Published {
            sequence_number: snapshot.sequence_number(),
            main_head: metadata
                .snapshot_for_ref(MAIN_BRANCH)
                .is_some_and(|head| head.snapshot_id() == self.snapshot_id),
        }
    }
}

impl CatalogConnection {
    /// Reloads `table`, without vending credentials, and looks for the
    /// expected snapshot.
    ///
    /// # Errors
    ///
    /// The errors of [`load_table`](Self::load_table). A missing table is an
    /// error, not [`SnapshotEvidence::Absent`].
    pub async fn snapshot_evidence(
        &self,
        table: &TableName,
        expected: &ExpectedSnapshot,
    ) -> Result<SnapshotEvidence, CatalogError> {
        let loaded = self.load_table(table, false).await?;
        Ok(expected.find_in(&loaded.metadata))
    }
}
