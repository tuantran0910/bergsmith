//! Rewrite proposals: the replacement snapshot a job publishes (BS-006.T1,
//! T4).

use std::collections::HashMap;

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{JobId, PublicationId};
use iceberg::TableUpdate;
use iceberg::spec::{DataContentType, DataFile, TableMetadata};
use iceberg::transaction::{ApplyTransactionAction, Transaction};
use uuid::Uuid;

use crate::commit::baseline::Baseline;
use crate::commit::error::ProposalError;
use crate::commit::evidence::ExpectedSnapshot;
use crate::commit::request::CommitRequest;

/// Snapshot summary property holding the Bergsmith job ID.
pub const JOB_ID_PROPERTY: &str = "bergsmith.job-id";
/// Snapshot summary property holding the Bergsmith publication ID.
pub const PUBLICATION_ID_PROPERTY: &str = "bergsmith.publication-id";
/// Snapshot summary property holding the proposal hash.
pub const PROPOSAL_HASH_PROPERTY: &str = "bergsmith.proposal-sha256";

/// The hash domain of a rewrite proposal.
const PROPOSAL_DOMAIN: &[u8] = b"bergsmith.catalog.v1.RewriteProposal";

/// A replacement of data files in the baseline `main` snapshot.
///
/// The new snapshot has an explicit ID, keeps the data sequence number of
/// the baseline `main` snapshot for added files, and carries the job ID,
/// publication ID, and proposal hash in its summary as recovery evidence.
#[derive(Clone, Debug)]
pub struct RewriteProposal {
    /// The job publishing the rewrite.
    pub job_id: JobId,
    /// The job's publication. Also the iceberg-rust commit UUID, so the
    /// manifests the commit writes are named after it.
    pub publication_id: PublicationId,
    /// The ID of the new snapshot; see [`new_snapshot_id`].
    pub snapshot_id: i64,
    /// Files to remove, as they appear in the baseline manifests.
    pub removed: Vec<DataFile>,
    /// Files to add.
    pub added: Vec<DataFile>,
}

/// A commit ready to persist and send.
#[derive(Clone, Debug)]
pub struct PreparedCommit {
    /// The exact request.
    pub request: CommitRequest,
    /// What a published snapshot must look like, to recognize it later.
    pub expected: ExpectedSnapshot,
}

impl RewriteProposal {
    /// The hash of everything the proposal commits to: table, job,
    /// publication, snapshot ID, baseline state, and the removed and added
    /// files. It is written into the snapshot summary, so it cannot cover
    /// the request that contains it; [`CommitRequest::sha256`] covers that.
    ///
    /// # Errors
    ///
    /// [`ProposalError::NoMainSnapshot`] when the baseline has no `main`
    /// snapshot.
    pub fn proposal_hash(&self, baseline: &Baseline) -> Result<Sha256Digest, ProposalError> {
        let main = baseline
            .main_snapshot()
            .ok_or(ProposalError::NoMainSnapshot)?;
        let metadata = baseline.metadata();
        let mut encoder = Encoder::default();
        encoder.uuid(metadata.uuid());
        encoder.uuid(*self.job_id.as_uuid());
        encoder.uuid(*self.publication_id.as_uuid());
        encoder.i64(self.snapshot_id);
        encoder.i64(main.snapshot_id());
        encoder.i64(main.sequence_number());
        encoder.i64(i64::from(metadata.current_schema_id()));
        encoder.i64(i64::from(metadata.last_column_id()));
        encoder.i64(i64::from(metadata.default_partition_spec_id()));
        encoder.i64(i64::from(metadata.last_partition_id()));
        encoder.i64(metadata.default_sort_order_id());
        encoder.files(&self.removed);
        encoder.files(&self.added);
        Ok(Sha256Digest::of(PROPOSAL_DOMAIN, &encoder.0))
    }

    /// Builds the commit on `baseline` with iceberg-rust's rewrite action,
    /// checking that every removed file is live in the baseline.
    ///
    /// Building writes the new manifests and manifest list under the table's
    /// metadata location; it does not change the table.
    ///
    /// # Errors
    ///
    /// [`ProposalError::NothingRemoved`], [`ProposalError::NoMainSnapshot`],
    /// [`ProposalError::SnapshotIdInUse`], [`ProposalError::Iceberg`] when
    /// iceberg-rust rejects the rewrite, and
    /// [`ProposalError::UnexpectedSnapshot`] when the result does not add the
    /// proposed snapshot on the baseline.
    pub async fn prepare(self, baseline: &Baseline) -> Result<PreparedCommit, ProposalError> {
        if self.removed.is_empty() {
            return Err(ProposalError::NothingRemoved);
        }
        if baseline
            .metadata()
            .snapshot_by_id(self.snapshot_id)
            .is_some()
        {
            return Err(ProposalError::SnapshotIdInUse(self.snapshot_id));
        }
        let proposal_hash = self.proposal_hash(baseline)?;
        let main = baseline
            .main_snapshot()
            .ok_or(ProposalError::NoMainSnapshot)?;
        let (parent, sequence_number) = (main.snapshot_id(), main.sequence_number());
        let summary = HashMap::from([
            (JOB_ID_PROPERTY.to_owned(), self.job_id.to_string()),
            (
                PUBLICATION_ID_PROPERTY.to_owned(),
                self.publication_id.to_string(),
            ),
            (PROPOSAL_HASH_PROPERTY.to_owned(), proposal_hash.to_hex()),
        ]);
        let snapshot_id = self.snapshot_id;
        let commit_uuid = *self.publication_id.as_uuid();
        let (updates, requirements) = baseline
            .capture(|transaction: Transaction| {
                let mut action = transaction
                    .rewrite_files()
                    .delete_files(self.removed)
                    .add_data_files(self.added)
                    .set_snapshot_id(snapshot_id)
                    .set_new_data_file_sequence_number(sequence_number)
                    // Rewrite authority covers selected data files only. Retain
                    // every delete, including obsolete ones, for later cleanup.
                    .set_enable_delete_filter_manager(false)
                    .set_check_file_existence(true);
                action.set_snapshot_properties(summary);
                action.set_commit_uuid(commit_uuid);
                action.apply(transaction)
            })
            .await?;
        let manifest_list = added_snapshot(&updates, snapshot_id, parent)
            .ok_or(ProposalError::UnexpectedSnapshot { snapshot_id })?;
        let expected = ExpectedSnapshot {
            table_uuid: baseline.metadata().uuid(),
            snapshot_id,
            parent_snapshot_id: Some(parent),
            manifest_list,
            job_id: self.job_id,
            publication_id: self.publication_id,
            proposal_hash,
        };
        let request = CommitRequest::encode(
            baseline.name(),
            baseline.identifier(),
            requirements,
            updates,
        )?;
        Ok(PreparedCommit { request, expected })
    }
}

/// The manifest list of the snapshot `updates` add with `snapshot_id` and
/// `parent`, if they add exactly that.
fn added_snapshot(updates: &[TableUpdate], snapshot_id: i64, parent: i64) -> Option<String> {
    updates.iter().find_map(|update| match update {
        TableUpdate::AddSnapshot { snapshot }
            if snapshot.snapshot_id() == snapshot_id
                && snapshot.parent_snapshot_id() == Some(parent) =>
        {
            Some(snapshot.manifest_list().to_owned())
        }
        _ => None,
    })
}

/// A random positive snapshot ID that `metadata` does not use.
#[must_use]
pub fn new_snapshot_id(metadata: &TableMetadata) -> i64 {
    loop {
        let (_, random) = Uuid::now_v7().as_u64_pair();
        if let Ok(id) = i64::try_from(random >> 1)
            && id != 0
            && metadata.snapshot_by_id(id).is_none()
        {
            return id;
        }
    }
}

/// A length-prefixed byte encoding for hashing.
#[derive(Default)]
struct Encoder(Vec<u8>);

impl Encoder {
    fn bytes(&mut self, bytes: &[u8]) {
        self.0
            .extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        self.0.extend_from_slice(bytes);
    }

    fn uuid(&mut self, uuid: Uuid) {
        self.bytes(uuid.as_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    /// Files sorted by path, each with its content type, partition spec,
    /// record count, and size.
    fn files(&mut self, files: &[DataFile]) {
        let mut sorted: Vec<&DataFile> = files.iter().collect();
        sorted.sort_by(|a, b| a.file_path().cmp(b.file_path()));
        self.u64(sorted.len() as u64);
        for file in sorted {
            self.bytes(file.file_path().as_bytes());
            self.0.push(match file.content_type() {
                DataContentType::Data => 0,
                DataContentType::PositionDeletes => 1,
                DataContentType::EqualityDeletes => 2,
            });
            self.i64(i64::from(file.partition_spec_id()));
            self.u64(file.record_count());
            self.u64(file.file_size_in_bytes());
        }
    }
}
