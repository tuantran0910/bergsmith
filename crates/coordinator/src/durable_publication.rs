//! Durable native publication and conservative crash recovery (BS-012.T1, T2, T4).
//!
//! Recovery never replays mutations. A send authorized before a crash can still
//! publish later, so absent evidence retains the table barrier and every output.

use std::collections::BTreeSet;
use std::time::Duration;

use bergsmith_catalog::{
    Baseline, CatalogConnection, CommitOutcome, CommitRequest, ExpectedSnapshot, PreparedCommit,
    ProposalError, RewriteProposal, SnapshotEvidence, TableName, new_snapshot_id,
};
use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{CatalogConnectionId, JobId, PublicationId};
use bergsmith_common::job::JobState;
use bergsmith_maintenance::PlanningBaseline;
use bergsmith_persistence::cancellation::SendAuthorization;
use bergsmith_persistence::jobs::Job;
use bergsmith_persistence::{PersistenceError, jobs, publications};
use bergsmith_storage::{Location, ObjectStore};
use iceberg::spec::{DataContentType, FormatVersion, ManifestList};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::publication::{PublicationError, send_first_commit};
use crate::results::{self, ResultError};

/// Why a durable publication operation could not complete.
#[derive(Debug, thiserror::Error)]
pub enum DurablePublicationError {
    /// The durable ledger rejected the operation.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// A receipt or output failed verification.
    #[error(transparent)]
    Result(#[from] ResultError),
    /// Proposal construction failed.
    #[error(transparent)]
    Proposal(#[from] ProposalError),
    /// Catalog metadata could not be read. The barrier is retained.
    #[error(transparent)]
    Catalog(#[from] bergsmith_catalog::CatalogError),
    /// Original manifest metadata could not be read.
    #[error("publication manifest metadata could not be read")]
    Iceberg(#[from] iceberg::Error),
    /// The original planning baseline changed; fresh planning is required.
    #[error("publication baseline changed for job {0}")]
    BaselineChanged(JobId),
    /// Stored intent failed identity or hash verification.
    #[error("publication intent is inconsistent for job {0}")]
    InvalidIntent(JobId),
    /// Encoding durable evidence failed.
    #[error("publication evidence encoding failed")]
    Encoding(#[from] serde_json::Error),
    /// Bounded object metadata verification exceeded its deadline.
    #[error("publication preparation timed out")]
    Timeout,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    catalog_connection_id: CatalogConnectionId,
    table_uuid: Uuid,
    namespace: Vec<String>,
    table_name: String,
    snapshot_id: i64,
    parent_snapshot_id: Option<i64>,
    manifest_list: String,
    proposal_hash: String,
    plan: serde_json::Value,
    receipts: Vec<(String, String)>,
    // No catalog has been certified for replay yet. Persist this policy explicitly.
    idempotency_key: Option<Uuid>,
}

/// Reloads and validates the original baseline, verifies all accepted results,
/// builds the exact request, then atomically persists it and enters `Committing`.
///
/// Manifest creation and object verification run outside database transactions.
/// A failed database write leaves harmless unreferenced metadata; it never sends.
///
/// # Errors
/// Returns baseline drift, invalid receipts, cancellation, stale revision/leadership,
/// or a table barrier conflict without authorizing a commit.
pub async fn prepare_publication(
    conn: &mut PgConnection,
    job: &Job,
    epoch: i64,
    catalog_connection_id: CatalogConnectionId,
    catalog: &CatalogConnection,
    baseline: &Baseline,
    store: &ObjectStore,
) -> Result<Job, DurablePublicationError> {
    publications::validate_destination(
        conn,
        job.id,
        &bergsmith_persistence::tables::TableAlias {
            connection_id: catalog_connection_id,
            namespace: baseline.name().namespace.clone(),
            name: baseline.name().name.clone(),
        },
        baseline.metadata().uuid(),
    )
    .await?;
    let plan = results::plan(conn, job.id).await?;
    let captured = PlanningBaseline::capture(baseline.metadata(), baseline.metadata_location())
        .map_err(ResultError::from)?;
    if captured.as_ref() != Some(plan.baseline()) || plan.job_id() != job.id {
        return Err(DurablePublicationError::BaselineChanged(job.id));
    }
    let current = catalog.load_table(baseline.name(), false).await?;
    if !baseline.changes(&current).is_empty() {
        return Err(DurablePublicationError::BaselineChanged(job.id));
    }
    let receipts = publications::accepted_receipts(conn, job.id).await?;
    if receipts.len() != plan.tasks().len() || receipts.is_empty() {
        return Err(ResultError::Invalid("every planned task needs an accepted receipt").into());
    }
    let work = build_proposal(&plan, &receipts, job.id, baseline, store);
    let prepared = tokio::time::timeout(Duration::from_secs(60), work)
        .await
        .map_err(|_| DurablePublicationError::Timeout)??;
    let expected = &prepared.expected;
    let evidence = Evidence {
        catalog_connection_id,
        table_uuid: expected.table_uuid,
        namespace: prepared.request.table().namespace.clone(),
        table_name: prepared.request.table().name.clone(),
        snapshot_id: expected.snapshot_id,
        parent_snapshot_id: expected.parent_snapshot_id,
        manifest_list: expected.manifest_list.clone(),
        proposal_hash: expected.proposal_hash.to_hex(),
        plan: plan.to_value().map_err(ResultError::from)?,
        receipts: receipts
            .iter()
            .map(|r| (r.attempt_id.to_string(), r.content_hash.clone()))
            .collect(),
        idempotency_key: None,
    };
    let intent = publications::Intent {
        job_id: job.id,
        publication_id: expected.publication_id,
        content: serde_json::to_value(evidence)?,
        request_body: prepared.request.body().to_vec(),
        request_hash: prepared.request.sha256().to_hex(),
    };
    Ok(publications::create_intent(conn, &intent, job.state_revision, epoch, &receipts).await?)
}

/// Restores the exact request and expected snapshot from the immutable intent.
///
/// # Errors
/// Returns `InvalidIntent` on inconsistent identities, policy, or request hash.
pub async fn load_prepared(
    conn: &mut PgConnection,
    job_id: JobId,
) -> Result<PreparedCommit, DurablePublicationError> {
    let intent = publications::intent(conn, job_id).await?;
    let evidence: Evidence = serde_json::from_value(intent.content)?;
    let request = CommitRequest::from_persisted(
        TableName {
            namespace: evidence.namespace,
            name: evidence.table_name,
        },
        intent.request_body,
    );
    if request.sha256().to_hex() != intent.request_hash || evidence.idempotency_key.is_some() {
        return Err(DurablePublicationError::InvalidIntent(job_id));
    }
    let expected = ExpectedSnapshot {
        table_uuid: evidence.table_uuid,
        snapshot_id: evidence.snapshot_id,
        parent_snapshot_id: evidence.parent_snapshot_id,
        manifest_list: evidence.manifest_list,
        job_id,
        publication_id: intent.publication_id,
        proposal_hash: Sha256Digest::from_hex(&evidence.proposal_hash)
            .map_err(|_| DurablePublicationError::InvalidIntent(job_id))?,
    };
    Ok(PreparedCommit { request, expected })
}

/// The persisted catalog connection to reopen when recovering this publication.
///
/// # Errors
/// Returns missing/invalid intent errors; it never obtains credentials or sends a commit.
pub async fn publication_connection(
    conn: &mut PgConnection,
    job_id: JobId,
) -> Result<CatalogConnectionId, DurablePublicationError> {
    let intent = publications::intent(conn, job_id).await?;
    let evidence: Evidence = serde_json::from_value(intent.content)?;
    Ok(evidence.catalog_connection_id)
}

/// Sends the persisted request once after authorization and durably records its outcome.
///
/// Only this live sender's conclusive first-request result proves non-publication.
/// A crash before recording it leaves an authorized request unresolved. No caller
/// may infer that a restored `Committing` job was never sent.
///
/// # Errors
/// Authorization/read/database errors preserve the existing durable intent and barrier.
pub async fn publish(
    conn: &mut PgConnection,
    job: &Job,
    epoch: i64,
    catalog: &CatalogConnection,
) -> Result<Job, DurablePublicationError> {
    let prepared = load_prepared(conn, job.id).await?;
    let current = jobs::job(conn, job.organization_id, job.id).await?;
    if current.publication_started_at.is_some() {
        return Err(PersistenceError::PublicationAlreadyAuthorized { job: job.id }.into());
    }
    if current.state != JobState::Committing {
        return Err(PersistenceError::JobNotInState {
            job: current.id,
            state: current.state,
            operation: "publish a durable intent",
        }
        .into());
    }
    // Recheck the original baseline even after a restart between intent creation
    // and authorization; standard requirements cannot assert metadata location/properties.
    let plan = results::plan(conn, job.id).await?;
    let loaded = catalog.load_table(prepared.request.table(), false).await?;
    let observed = PlanningBaseline::capture(&loaded.metadata, loaded.metadata_location.as_deref())
        .map_err(ResultError::from)?;
    if observed.as_ref() != Some(plan.baseline()) {
        return Ok(publications::resolve(
            conn,
            current.id,
            current.state_revision,
            JobState::ReplanRequired,
            "PublicationBaselineChanged",
            false,
        )
        .await?);
    }
    let sent = send_first_commit(
        conn,
        SendAuthorization {
            job_id: job.id,
            expected_revision: job.state_revision,
            coordinator_epoch: epoch,
        },
        catalog,
        &prepared.request,
        None,
    )
    .await;
    let (to, reason, proof) = match sent {
        Ok(sent) => match sent.outcome {
            CommitOutcome::Committed(_) => (JobState::Succeeded, "CommitConfirmed", false),
            CommitOutcome::Unknown(_) => (
                JobState::CommitOutcomeUnknown,
                "CommitOutcomeUnknown",
                false,
            ),
            CommitOutcome::Rejected(rejection) => {
                let state = if matches!(
                    rejection.kind,
                    bergsmith_catalog::RejectionKind::RequirementFailed
                        | bergsmith_catalog::RejectionKind::NotFound
                ) {
                    JobState::ReplanRequired
                } else {
                    JobState::Failed
                };
                (state, "CommitRejected", true)
            }
        },
        Err(PublicationError::NotAuthorized(error)) => return Err(error.into()),
        Err(PublicationError::NotSent { .. }) => (JobState::Failed, "CommitNotSent", true),
    };
    let current = jobs::job(conn, job.organization_id, job.id).await?;
    if current.state.is_terminal() {
        return Ok(current);
    }
    if current.state == to {
        return Ok(current);
    }
    Ok(publications::resolve(conn, current.id, current.state_revision, to, reason, proof).await?)
}

/// Reconciles a restored authorized job through retained snapshot evidence only.
///
/// Matching evidence succeeds even when a later snapshot is current or cancellation
/// was requested. Missing/mismatched evidence remains unknown with its barrier;
/// catalog failure leaves state and outputs intact for another read-only attempt.
/// An intent with no authorization can still be sent through [`publish`] or cancelled.
///
/// # Errors
/// Returns catalog, intent, or revision errors without inventing non-publication proof.
pub async fn reconcile(
    conn: &mut PgConnection,
    job: &Job,
    catalog: &CatalogConnection,
) -> Result<Job, DurablePublicationError> {
    let current = jobs::job(conn, job.organization_id, job.id).await?;
    if current.state == JobState::Cancelling && current.publication_started_at.is_none() {
        // An intent's tasks were already accepted before Committing. No active
        // attempt remains to stop, and cancellation prevents any authorization.
        publications::intent(conn, job.id).await?;
        return Ok(publications::resolve(
            conn,
            current.id,
            current.state_revision,
            JobState::Cancelled,
            "CancellationRequested",
            false,
        )
        .await?);
    }
    if current.state.is_terminal() || current.publication_started_at.is_none() {
        return Ok(current);
    }
    let prepared = load_prepared(conn, job.id).await?;
    let evidence = catalog
        .snapshot_evidence(prepared.request.table(), &prepared.expected)
        .await?;
    let (state, reason) = match evidence {
        SnapshotEvidence::Published { .. } => (JobState::Succeeded, "SnapshotEvidenceMatched"),
        SnapshotEvidence::Absent => (JobState::CommitOutcomeUnknown, "SnapshotEvidenceAbsent"),
        SnapshotEvidence::Mismatch(_) => {
            (JobState::CommitOutcomeUnknown, "SnapshotEvidenceMismatch")
        }
    };
    if current.state == state {
        return Ok(current);
    }
    Ok(publications::resolve(
        conn,
        current.id,
        current.state_revision,
        state,
        reason,
        false,
    )
    .await?)
}

async fn build_proposal(
    plan: &bergsmith_maintenance::PlanDocument,
    receipts: &[publications::AcceptedReceipt],
    job_id: JobId,
    baseline: &Baseline,
    store: &ObjectStore,
) -> Result<PreparedCommit, DurablePublicationError> {
    let mut added = Vec::new();
    for (accepted, task) in receipts.iter().zip(plan.tasks()) {
        let receipt = results::decode(accepted)?;
        let artifact = plan.artifact_of(task).map_err(ResultError::from)?;
        let prefix = Location::parse(plan.baseline().data_location())
            .map_err(ResultError::from)?
            .attempt_output_prefix(job_id, accepted.attempt_id);
        added.extend(
            results::verify(&artifact, &receipt, &prefix, store, baseline.metadata()).await?,
        );
    }
    let selected: BTreeSet<_> = plan
        .tasks()
        .iter()
        .flat_map(|t| {
            t.sources()
                .iter()
                .map(bergsmith_maintenance::PlannedFile::path)
        })
        .collect();
    let snapshot = baseline
        .metadata()
        .snapshot_by_id(plan.baseline().snapshot_id())
        .ok_or(DurablePublicationError::InvalidIntent(job_id))?;
    let bytes = store
        .file_io()
        .new_input(snapshot.manifest_list())?
        .read()
        .await?;
    let list = ManifestList::parse_with_version(&bytes, FormatVersion::V2)?;
    let mut removed = Vec::new();
    for manifest in list.entries() {
        let loaded = manifest.load_manifest(store.file_io()).await?;
        for entry in loaded.entries().iter().filter(|e| e.is_alive()) {
            let file = entry.data_file();
            if file.content_type() == DataContentType::Data && selected.contains(file.file_path()) {
                removed.push(file.clone());
            }
        }
    }
    if removed.len() != selected.len() {
        return Err(DurablePublicationError::InvalidIntent(job_id));
    }
    Ok(RewriteProposal {
        job_id,
        publication_id: PublicationId::generate(),
        snapshot_id: new_snapshot_id(baseline.metadata()),
        removed,
        added,
    }
    .prepare(baseline)
    .await?)
}
