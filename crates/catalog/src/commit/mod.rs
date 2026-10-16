//! The standard REST conditional commit adapter (BS-006).
//!
//! A commit is built once from the planning [`Baseline`] with iceberg-rust's
//! rewrite action, asserts the baseline's full requirement set, is
//! serialized once into a [`CommitRequest`] whose exact bytes are hashed and
//! persisted, and is sent at most once per call with an optional
//! [`IdempotencyKey`]. Bergsmith never calls iceberg-rust's
//! `Transaction::commit` against a real catalog (ADR 0015).

mod baseline;
mod capture;
mod error;
mod evidence;
mod proposal;
mod request;
mod send;
#[cfg(test)]
mod tests;

pub use baseline::Baseline;
pub use error::ProposalError;
pub use evidence::{EvidenceMismatch, ExpectedSnapshot, SnapshotEvidence};
pub use proposal::{
    JOB_ID_PROPERTY, PROPOSAL_HASH_PROPERTY, PUBLICATION_ID_PROPERTY, PreparedCommit,
    RewriteProposal, new_snapshot_id,
};
pub use request::CommitRequest;
pub use send::{
    CommitOutcome, CommitRejection, Committed, IdempotencyKey, RejectionKind, UnknownOutcome,
};
