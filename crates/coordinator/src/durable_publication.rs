//! Durable native publication and conservative crash recovery (BS-012.T1, T2, T4).
//!
//! Recovery never replays mutations. A send authorized before a crash can still
//! publish later, so absent evidence retains the table barrier and every output.

use std::collections::BTreeSet;
use std::time::Duration;

use bergsmith_catalog::{
    Baseline, CatalogConnection, CommitOutcome, CommitRequest, ExpectedSnapshot,
    ManifestRewriteProposal, PreparedCommit, ProposalError, RewriteProposal, SnapshotEvidence,
    TableName, new_snapshot_id,
};
use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{CatalogConnectionId, JobId, PublicationId};
use bergsmith_common::job::{JobOperation, JobState};
use bergsmith_maintenance::metadata_plan::{MetadataPlanDocument, MetadataWork};
use bergsmith_maintenance::{ExpirationPlanDocument, PlanningBaseline};
use bergsmith_persistence::cancellation::SendAuthorization;
use bergsmith_persistence::jobs::Job;
use bergsmith_persistence::{PersistenceError, jobs, publications};
use bergsmith_storage::{Location, ObjectStore};
use iceberg::TableUpdate;
use iceberg::spec::{DataContentType, FormatVersion, ManifestContentType, ManifestList};

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
    /// A ledger-protected snapshot appeared in the expiration set.
    #[error("expiration for job {0} would remove a protected snapshot")]
    ProtectedSnapshot(JobId),
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

/// Evidence for a metadata-only `remove-snapshots` commit.
///
/// There is no proposed snapshot to find. Recovery instead requires the same
/// `main` head, every listed snapshot absent, and every protected snapshot
/// present (ADR 0061).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpirationEvidence {
    catalog_connection_id: CatalogConnectionId,
    table_uuid: Uuid,
    namespace: Vec<String>,
    table_name: String,
    baseline_snapshot_id: i64,
    remove: Vec<i64>,
    protected: Vec<i64>,
}

/// The connection field shared by rewrite and expiration intents.
#[derive(Deserialize)]
struct IntentConnection {
    catalog_connection_id: CatalogConnectionId,
}

/// Bounds for publication preparation, applied to each storage step separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicationConfig {
    /// Deadline for one bounded step: verifying one task's accepted outputs,
    /// reading the baseline manifest list, loading one baseline manifest, or
    /// writing the proposal's manifests. The whole preparation therefore
    /// scales with the job instead of sharing one fixed deadline.
    pub operation_timeout: Duration,
}

impl Default for PublicationConfig {
    fn default() -> Self {
        Self {
            operation_timeout: Duration::from_secs(60),
        }
    }
}

/// [`prepare_publication_with`] using [`PublicationConfig::default`].
///
/// # Errors
/// See [`prepare_publication_with`].
pub async fn prepare_publication(
    conn: &mut PgConnection,
    job: &Job,
    epoch: i64,
    catalog_connection_id: CatalogConnectionId,
    catalog: &CatalogConnection,
    baseline: &Baseline,
    store: &ObjectStore,
) -> Result<Job, DurablePublicationError> {
    prepare_publication_with(
        conn,
        job,
        epoch,
        catalog_connection_id,
        catalog,
        baseline,
        store,
        PublicationConfig::default(),
    )
    .await
}

/// Reloads and validates the original baseline, verifies all accepted results,
/// builds the exact request, then atomically persists it and enters `Committing`.
///
/// Manifest creation and object verification run outside database transactions.
/// Every storage step is bounded by `config.operation_timeout`; catalog calls
/// are bounded by the connection's request timeout. A failed database write
/// leaves harmless unreferenced metadata; it never sends. A scheduled job whose
/// policy was disabled resolves as `Failed` before an intent is written.
///
/// # Errors
/// Returns baseline drift, invalid receipts, cancellation, stale revision/leadership,
/// a step timeout, or a table barrier conflict without authorizing a commit.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is an independent durable or external input to one atomic preparation"
)]
pub async fn prepare_publication_with(
    conn: &mut PgConnection,
    job: &Job,
    epoch: i64,
    catalog_connection_id: CatalogConnectionId,
    catalog: &CatalogConnection,
    baseline: &Baseline,
    store: &ObjectStore,
    config: PublicationConfig,
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
    let prepared = build_proposal(
        &plan,
        &receipts,
        job.id,
        baseline,
        store,
        config.operation_timeout,
    )
    .await?;
    persist_snapshot_intent(
        conn,
        job,
        epoch,
        catalog_connection_id,
        &prepared,
        plan.to_value().map_err(ResultError::from)?,
        &receipts,
    )
    .await
}

/// Persists the intent of a commit that adds one snapshot on the baseline
/// `main` and enters `Committing`.
///
/// A deployment policy disabled before the intent exists proves
/// non-publication, so it fails the job instead of leaving it ready.
async fn persist_snapshot_intent(
    conn: &mut PgConnection,
    job: &Job,
    epoch: i64,
    catalog_connection_id: CatalogConnectionId,
    prepared: &PreparedCommit,
    plan: serde_json::Value,
    receipts: &[publications::AcceptedReceipt],
) -> Result<Job, DurablePublicationError> {
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
        plan,
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
    match publications::create_intent(conn, &intent, job.state_revision, epoch, receipts).await {
        Ok(committing) => Ok(committing),
        Err(PersistenceError::MaintenancePolicyDisabled { .. }) => {
            // No intent or send exists, so the denial proves non-publication.
            // The job is still `ReadyToCommit`; failing it releases its table
            // for later occurrences instead of leaving them coalesced forever.
            let reason = "PolicyDisabled";
            let resolved = publications::resolve_pre_intent_denial(
                conn,
                job.id,
                job.state_revision,
                epoch,
                reason,
            )
            .await?;
            record_publication_decision(
                conn,
                job,
                bergsmith_common::audit::AuditAction::JobPublicationDenied,
                bergsmith_common::audit::AuditOutcome::Denied,
                reason,
            )
            .await;
            match resolved {
                Some(failed) => Ok(failed),
                None => Err(PersistenceError::MaintenancePolicyDisabled { job: job.id }.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

/// Prepares a manifest rewrite or dangling-delete removal (ADR 0063) and
/// persists its intent, then enters `Committing`.
///
/// The original baseline is required, exactly as for a compaction: the stored
/// plan's baseline must equal both `baseline` and a fresh catalog load. The
/// proposal (reading the baseline manifests and writing the new ones) runs
/// under one deadline, `limit`, because iceberg-rust's rewrite action is one
/// step. The intent has the compaction shape with no receipts, so send,
/// snapshot-evidence recovery, and attestation are shared.
///
/// # Errors
///
/// Returns baseline drift, an inconsistent plan, a missed deadline,
/// cancellation, a stale revision or leadership, or a table barrier conflict
/// without authorizing a commit.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is an independent durable or external input to one atomic preparation"
)]
pub async fn prepare_metadata_rewrite(
    conn: &mut PgConnection,
    job: &Job,
    epoch: i64,
    catalog_connection_id: CatalogConnectionId,
    catalog: &CatalogConnection,
    baseline: &Baseline,
    store: &ObjectStore,
    plan: &MetadataPlanDocument,
    limit: Duration,
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
    let captured = PlanningBaseline::capture(baseline.metadata(), baseline.metadata_location())
        .map_err(ResultError::from)?;
    if captured.as_ref() != Some(&plan.baseline) || plan.job_id != job.id {
        return Err(DurablePublicationError::BaselineChanged(job.id));
    }
    let current = catalog.load_table(baseline.name(), false).await?;
    if !baseline.changes(&current).is_empty() {
        return Err(DurablePublicationError::BaselineChanged(job.id));
    }
    let prepared = bounded(limit, metadata_proposal(plan, job.id, baseline, store)).await?;
    persist_snapshot_intent(
        conn,
        job,
        epoch,
        catalog_connection_id,
        &prepared,
        plan.to_value().map_err(ResultError::from)?,
        &[],
    )
    .await
}

/// Builds the commit a metadata plan describes.
async fn metadata_proposal(
    plan: &MetadataPlanDocument,
    job_id: JobId,
    baseline: &Baseline,
    store: &ObjectStore,
) -> Result<PreparedCommit, DurablePublicationError> {
    let publication_id = PublicationId::generate();
    let snapshot_id = new_snapshot_id(baseline.metadata());
    match &plan.work {
        MetadataWork::RewriteManifests {
            target_size_bytes,
            manifests,
            ..
        } => Ok(ManifestRewriteProposal {
            job_id,
            publication_id,
            snapshot_id,
            manifests: manifests.iter().map(|m| m.path.clone()).collect(),
            target_size_bytes: *target_size_bytes,
        }
        .prepare(baseline)
        .await?),
        MetadataWork::RemoveDanglingDeletes { deletes, .. } => {
            let planned: BTreeSet<&str> = deletes.iter().map(|d| d.path.as_str()).collect();
            let snapshot = baseline
                .metadata()
                .snapshot_by_id(plan.baseline.snapshot_id())
                .ok_or(DurablePublicationError::InvalidIntent(job_id))?;
            let bytes = store
                .file_io()
                .new_input(snapshot.manifest_list())?
                .read()
                .await?;
            let list = ManifestList::parse_with_version(&bytes, FormatVersion::V2)?;
            let mut removed = Vec::new();
            for manifest in list.entries() {
                if manifest.content != ManifestContentType::Deletes {
                    continue;
                }
                let loaded = manifest.load_manifest(store.file_io()).await?;
                for entry in loaded.entries().iter().filter(|e| e.is_alive()) {
                    let file = entry.data_file();
                    if file.content_type() != DataContentType::Data
                        && planned.contains(file.file_path())
                    {
                        removed.push(file.clone());
                    }
                }
            }
            if removed.len() != planned.len() {
                return Err(DurablePublicationError::InvalidIntent(job_id));
            }
            Ok(RewriteProposal {
                job_id,
                publication_id,
                snapshot_id,
                removed,
                added: Vec::new(),
            }
            .prepare(baseline)
            .await?)
        }
    }
}

/// Reads and verifies one job's immutable metadata plan.
///
/// # Errors
///
/// [`DurablePublicationError::InvalidIntent`] when the row is missing, does
/// not decode, belongs to another job, or does not match its hash.
pub async fn load_metadata_plan(
    conn: &mut PgConnection,
    job_id: JobId,
) -> Result<MetadataPlanDocument, DurablePublicationError> {
    let Some((content, hash)) =
        bergsmith_persistence::metadata_plans::metadata_plan(conn, job_id).await?
    else {
        return Err(DurablePublicationError::InvalidIntent(job_id));
    };
    let plan = MetadataPlanDocument::from_persisted(content)
        .map_err(|_| DurablePublicationError::InvalidIntent(job_id))?;
    let matches = plan
        .content_hash()
        .map_err(|_| DurablePublicationError::InvalidIntent(job_id))?
        .to_hex()
        == hash;
    if !matches || plan.job_id != job_id {
        return Err(DurablePublicationError::InvalidIntent(job_id));
    }
    Ok(plan)
}

/// The baseline a snapshot-adding job was planned from.
async fn planned_baseline(
    conn: &mut PgConnection,
    job: &Job,
) -> Result<PlanningBaseline, DurablePublicationError> {
    if job.operation.is_metadata_rewrite() {
        return Ok(load_metadata_plan(conn, job.id).await?.baseline);
    }
    Ok(results::plan(conn, job.id).await?.baseline().clone())
}

/// Prepares and persists a metadata-only expiration intent, then enters `Committing`.
///
/// The ledger-protected set is re-read here: a job that pinned a snapshot
/// after planning must not have it removed. A non-empty intersection ends in
/// `ReplanRequired` at the caller.
///
/// # Errors
///
/// Returns an invalid baseline, a newly protected snapshot, a stale revision,
/// lost leadership, or a table barrier conflict without sending anything.
pub async fn prepare_expiration(
    conn: &mut PgConnection,
    job: &Job,
    epoch: i64,
    catalog_connection_id: CatalogConnectionId,
    baseline: &Baseline,
    plan: &ExpirationPlanDocument,
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
    let baseline_snapshot_id = baseline
        .main_snapshot()
        .ok_or(ProposalError::NoMainSnapshot)?
        .snapshot_id();
    let request = baseline.commit_with_updates(vec![TableUpdate::RemoveSnapshots {
        snapshot_ids: plan.remove.clone(),
    }])?;
    let evidence = ExpirationEvidence {
        catalog_connection_id,
        table_uuid: baseline.metadata().uuid(),
        namespace: baseline.name().namespace.clone(),
        table_name: baseline.name().name.clone(),
        baseline_snapshot_id,
        remove: plan.remove.clone(),
        protected: plan.protected.clone(),
    };
    let intent = publications::Intent {
        job_id: job.id,
        publication_id: PublicationId::generate(),
        content: serde_json::to_value(evidence)?,
        request_body: request.body().to_vec(),
        request_hash: request.sha256().to_hex(),
    };
    match publications::create_expiration_intent(
        conn,
        &intent,
        job.state_revision,
        epoch,
        &plan.remove,
    )
    .await
    {
        Ok(job) => Ok(job),
        Err(PersistenceError::SnapshotPinned { .. }) => {
            Err(DurablePublicationError::ProtectedSnapshot(job.id))
        }
        Err(error) => Err(error.into()),
    }
}

/// Reads and verifies one job's immutable expiration plan.
///
/// # Errors
///
/// [`DurablePublicationError::InvalidIntent`] when the row is missing or its
/// hash does not match the stored content.
pub async fn load_expiration_plan(
    conn: &mut PgConnection,
    job_id: JobId,
) -> Result<ExpirationPlanDocument, DurablePublicationError> {
    let Some((content, hash)) =
        bergsmith_persistence::expirations::expiration_plan(conn, job_id).await?
    else {
        return Err(DurablePublicationError::InvalidIntent(job_id));
    };
    let plan = ExpirationPlanDocument::from_persisted(content)
        .map_err(|_| DurablePublicationError::InvalidIntent(job_id))?;
    if plan
        .content_hash()
        .map_err(|_| DurablePublicationError::InvalidIntent(job_id))?
        .to_hex()
        != hash
    {
        return Err(DurablePublicationError::InvalidIntent(job_id));
    }
    Ok(plan)
}

/// Reads the persisted expiration intent and its exact request.
async fn load_expiration(
    conn: &mut PgConnection,
    job_id: JobId,
) -> Result<(CommitRequest, ExpirationEvidence), DurablePublicationError> {
    let intent = publications::intent(conn, job_id).await?;
    let evidence: ExpirationEvidence = serde_json::from_value(intent.content)?;
    let request = CommitRequest::from_persisted(
        TableName {
            namespace: evidence.namespace.clone(),
            name: evidence.table_name.clone(),
        },
        intent.request_body,
    );
    if request.sha256().to_hex() != intent.request_hash {
        return Err(DurablePublicationError::InvalidIntent(job_id));
    }
    Ok((request, evidence))
}

/// Sends the persisted expiration request once after authorization.
///
/// # Errors
///
/// Authorization, catalog, or database errors preserve the intent and barrier.
#[expect(
    clippy::too_many_lines,
    reason = "one audited metadata publication path: reload, send, and outcome mapping"
)]
pub async fn publish_expiration(
    conn: &mut PgConnection,
    job: &Job,
    epoch: i64,
    catalog: &CatalogConnection,
) -> Result<Job, DurablePublicationError> {
    let (request, _) = load_expiration(conn, job.id).await?;
    let current = jobs::job(conn, job.organization_id, job.id).await?;
    if current.publication_started_at.is_some() {
        return Err(PersistenceError::PublicationAlreadyAuthorized { job: job.id }.into());
    }
    if current.state != JobState::Committing {
        return Err(PersistenceError::JobNotInState {
            job: current.id,
            state: current.state,
            operation: "publish an expiration intent",
        }
        .into());
    }
    let plan = load_expiration_plan(conn, job.id).await?;
    let loaded = catalog.load_table(request.table(), false).await?;
    let observed = PlanningBaseline::capture(&loaded.metadata, loaded.metadata_location.as_deref())
        .map_err(ResultError::from)?;
    if observed.as_ref() != Some(&plan.baseline) {
        return Ok(publications::resolve(
            conn,
            current.id,
            current.state_revision,
            epoch,
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
        &request,
        None,
    )
    .await;
    let (to, reason, proof) = match sent {
        Ok(sent) => match sent.outcome {
            CommitOutcome::Committed(_) => {
                record_publication_decision(
                    conn,
                    job,
                    bergsmith_common::audit::AuditAction::JobPublished,
                    bergsmith_common::audit::AuditOutcome::Allowed,
                    "CommitConfirmed",
                )
                .await;
                (JobState::Succeeded, "CommitConfirmed", false)
            }
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
        Err(PublicationError::NotAuthorized(PersistenceError::PermissionDenied { .. })) => {
            let resolved = Box::pin(publications::resolve_authorization_denied(
                conn,
                job.id,
                job.state_revision,
                epoch,
            ))
            .await?;
            record_publication_decision(
                conn,
                job,
                bergsmith_common::audit::AuditAction::JobPublicationDenied,
                bergsmith_common::audit::AuditOutcome::Denied,
                "AuthorizationRevoked",
            )
            .await;
            return Ok(match resolved {
                Some(resolved) => resolved,
                None => jobs::job(conn, job.organization_id, job.id).await?,
            });
        }
        Err(PublicationError::NotAuthorized(PersistenceError::MaintenancePolicyDisabled {
            ..
        })) => {
            let reason = "PolicyDisabled";
            let resolved = Box::pin(publications::resolve_publication_denied(
                conn,
                job.id,
                job.state_revision,
                epoch,
                reason,
            ))
            .await?;
            record_publication_decision(
                conn,
                job,
                bergsmith_common::audit::AuditAction::JobPublicationDenied,
                bergsmith_common::audit::AuditOutcome::Denied,
                reason,
            )
            .await;
            return Ok(match resolved {
                Some(resolved) => resolved,
                None => jobs::job(conn, job.organization_id, job.id).await?,
            });
        }
        Err(PublicationError::NotAuthorized(error)) => return Err(error.into()),
        Err(PublicationError::NotPrepared(error)) => return Err(error.into()),
        Err(PublicationError::NotSent { .. }) => (JobState::Failed, "CommitNotSent", true),
    };
    let current = jobs::job(conn, job.organization_id, job.id).await?;
    if current.state.is_terminal() {
        return Ok(current);
    }
    if current.state == to {
        return Ok(current);
    }
    Ok(publications::resolve(
        conn,
        current.id,
        current.state_revision,
        epoch,
        to,
        reason,
        proof,
    )
    .await?)
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
    // Rewrite and expiration evidence share this one field; parsing only it
    // keeps recovery independent of the operation's own shape.
    let intent: IntentConnection = serde_json::from_value(intent.content)?;
    Ok(intent.catalog_connection_id)
}

/// Sends the persisted request once after authorization and durably records its outcome.
///
/// Only this live sender's conclusive first-request result proves non-publication.
/// A crash before recording it leaves an authorized request unresolved. No caller
/// may infer that a restored `Committing` job was never sent.
///
/// The request is authenticated (including any OAuth2 token fetch) before send
/// authorization. A failure there leaves the job `Committing` and unauthorized,
/// so a later call may publish the same intent. Once authorized, only a
/// connection failure proves the request never left; that ends the job `Failed`
/// (or its pending/cancellation disposition), because authorization is one-shot.
///
/// # Errors
/// Authorization/read/database errors, and catalog errors before authorization,
/// preserve the existing durable intent and barrier.
#[expect(
    clippy::too_many_lines,
    reason = "one audited publication decision path: baseline recheck, send, and outcome mapping"
)]
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
    let planned = planned_baseline(conn, &current).await?;
    let loaded = catalog.load_table(prepared.request.table(), false).await?;
    let observed = PlanningBaseline::capture(&loaded.metadata, loaded.metadata_location.as_deref())
        .map_err(ResultError::from)?;
    if observed.as_ref() != Some(&planned) {
        return Ok(publications::resolve(
            conn,
            current.id,
            current.state_revision,
            epoch,
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
            CommitOutcome::Committed(_) => {
                record_publication_decision(
                    conn,
                    job,
                    bergsmith_common::audit::AuditAction::JobPublished,
                    bergsmith_common::audit::AuditOutcome::Allowed,
                    "CommitConfirmed",
                )
                .await;
                (JobState::Succeeded, "CommitConfirmed", false)
            }
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
        Err(PublicationError::NotAuthorized(PersistenceError::PermissionDenied { .. })) => {
            // Resolve atomically: if another sender was authorized after this
            // denial, it owns the barrier and this attempt must not release it.
            // Boxed to keep the publication future small; the resolve state
            // machine is not on the common path.
            let resolved = Box::pin(publications::resolve_authorization_denied(
                conn,
                job.id,
                job.state_revision,
                epoch,
            ))
            .await?;
            record_publication_decision(
                conn,
                job,
                bergsmith_common::audit::AuditAction::JobPublicationDenied,
                bergsmith_common::audit::AuditOutcome::Denied,
                "AuthorizationRevoked",
            )
            .await;
            return Ok(match resolved {
                Some(resolved) => resolved,
                None => jobs::job(conn, job.organization_id, job.id).await?,
            });
        }
        Err(PublicationError::NotAuthorized(PersistenceError::MaintenancePolicyDisabled {
            ..
        })) => {
            // The policy was disabled after admission and before send
            // authorization. Nothing was sent, so non-publication is proven.
            let reason = "PolicyDisabled";
            let resolved = Box::pin(publications::resolve_publication_denied(
                conn,
                job.id,
                job.state_revision,
                epoch,
                reason,
            ))
            .await?;
            record_publication_decision(
                conn,
                job,
                bergsmith_common::audit::AuditAction::JobPublicationDenied,
                bergsmith_common::audit::AuditOutcome::Denied,
                reason,
            )
            .await;
            return Ok(match resolved {
                Some(resolved) => resolved,
                None => jobs::job(conn, job.organization_id, job.id).await?,
            });
        }
        Err(PublicationError::NotAuthorized(error)) => return Err(error.into()),
        Err(PublicationError::NotPrepared(error)) => return Err(error.into()),
        Err(PublicationError::NotSent { .. }) => (JobState::Failed, "CommitNotSent", true),
    };
    let current = jobs::job(conn, job.organization_id, job.id).await?;
    if current.state.is_terminal() {
        return Ok(current);
    }
    if current.state == to {
        return Ok(current);
    }
    Ok(publications::resolve(
        conn,
        current.id,
        current.state_revision,
        epoch,
        to,
        reason,
        proof,
    )
    .await?)
}

/// Reconciles a restored authorized job through retained snapshot evidence only.
///
/// Matching evidence succeeds even when a later snapshot is current or cancellation
/// was requested. Missing/mismatched evidence remains unknown with its barrier;
/// catalog failure leaves state and outputs intact for another read-only attempt.
/// An intent with no authorization can still be sent through [`publish`] or cancelled.
/// Every durable write is fenced by `epoch`.
///
/// # Errors
/// Returns catalog, intent, leadership, or revision errors without inventing
/// non-publication proof.
pub async fn reconcile(
    conn: &mut PgConnection,
    job: &Job,
    epoch: i64,
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
            epoch,
            JobState::Cancelled,
            "CancellationRequested",
            false,
        )
        .await?);
    }
    if current.state.is_terminal() || current.publication_started_at.is_none() {
        return Ok(current);
    }
    let (state, reason) = match observe(conn, job.id, catalog).await? {
        SnapshotEvidence::Published { .. } => (JobState::Succeeded, "SnapshotEvidenceMatched"),
        SnapshotEvidence::ExpirationApplied => (JobState::Succeeded, "ExpirationEvidenceMatched"),
        SnapshotEvidence::Absent | SnapshotEvidence::ExpirationPending => {
            (JobState::CommitOutcomeUnknown, "SnapshotEvidenceAbsent")
        }
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
        epoch,
        state,
        reason,
        false,
    )
    .await?)
}

/// Operator resolution of an unknown outcome after external investigation.
///
/// The operator attests that no authorized request for this intent published
/// and that none can still publish, for example from catalog request logs.
/// Retained evidence is rechecked first, read-only: a matching snapshot always
/// wins and records `Succeeded`, whatever was attested. Otherwise the job takes
/// its cancellation or pending disposition, or `Failed`, and the transition
/// event records `actor` and `OperatorAttestedNonPublication`. Either way the
/// `job.non_publication_attested` audit record, with `evidence`, commits with
/// the transition. Catalog failure changes nothing. Nothing is ever resent.
///
/// # Errors
/// Rejects a job that is not `CommitOutcomeUnknown`, a changed revision,
/// lost leadership, unavailable catalog evidence, or a failed audit record,
/// without changing state.
pub async fn attest_non_publication(
    conn: &mut PgConnection,
    job: &Job,
    epoch: i64,
    catalog: &CatalogConnection,
    actor: bergsmith_common::ids::PrincipalId,
    evidence: &str,
) -> Result<Job, DurablePublicationError> {
    let current = jobs::job(conn, job.organization_id, job.id).await?;
    if current.state != JobState::CommitOutcomeUnknown
        || current.state_revision != job.state_revision
    {
        return Err(PersistenceError::JobNotInState {
            job: current.id,
            state: current.state,
            operation: "attest non-publication at the inspected revision",
        }
        .into());
    }
    let snapshot_matched = matches!(
        observe(conn, job.id, catalog).await?,
        SnapshotEvidence::Published { .. } | SnapshotEvidence::ExpirationApplied
    );
    Ok(publications::attest_non_publication(
        conn,
        &publications::Attestation {
            job_id: current.id,
            expected_revision: current.state_revision,
            epoch,
            actor,
            evidence,
            snapshot_matched,
        },
    )
    .await?)
}

/// Reads retained evidence for a job's immutable intent.
async fn observe(
    conn: &mut PgConnection,
    job_id: JobId,
    catalog: &CatalogConnection,
) -> Result<SnapshotEvidence, DurablePublicationError> {
    let job = jobs::job_by_id(conn, job_id).await?;
    if job.operation == JobOperation::ExpireSnapshots {
        let (request, evidence) = load_expiration(conn, job_id).await?;
        let loaded = catalog.load_table(request.table(), false).await?;
        if loaded.metadata.uuid() != evidence.table_uuid {
            return Ok(SnapshotEvidence::Mismatch(
                bergsmith_catalog::EvidenceMismatch::TableUuid(loaded.metadata.uuid()),
            ));
        }
        if loaded.metadata.current_snapshot_id() != Some(evidence.baseline_snapshot_id) {
            return Ok(SnapshotEvidence::ExpirationPending);
        }
        if evidence
            .remove
            .iter()
            .any(|snapshot_id| loaded.metadata.snapshot_by_id(*snapshot_id).is_some())
        {
            return Ok(SnapshotEvidence::ExpirationPending);
        }
        if evidence
            .protected
            .iter()
            .any(|snapshot_id| loaded.metadata.snapshot_by_id(*snapshot_id).is_none())
        {
            return Ok(SnapshotEvidence::ExpirationPending);
        }
        return Ok(SnapshotEvidence::ExpirationApplied);
    }
    let prepared = load_prepared(conn, job_id).await?;
    Ok(catalog
        .snapshot_evidence(prepared.request.table(), &prepared.expected)
        .await?)
}

/// Runs one storage step under its own deadline.
async fn bounded<T, E>(
    limit: Duration,
    step: impl Future<Output = Result<T, E>>,
) -> Result<T, DurablePublicationError>
where
    DurablePublicationError: From<E>,
{
    Ok(tokio::time::timeout(limit, step)
        .await
        .map_err(|_| DurablePublicationError::Timeout)??)
}

async fn build_proposal(
    plan: &bergsmith_maintenance::PlanDocument,
    receipts: &[publications::AcceptedReceipt],
    job_id: JobId,
    baseline: &Baseline,
    store: &ObjectStore,
    limit: Duration,
) -> Result<PreparedCommit, DurablePublicationError> {
    let mut added = Vec::new();
    for (accepted, task) in receipts.iter().zip(plan.tasks()) {
        let receipt = results::decode(accepted)?;
        let artifact = plan.artifact_of(task).map_err(ResultError::from)?;
        let prefix = Location::parse(plan.baseline().data_location())
            .map_err(ResultError::from)?
            .attempt_output_prefix(job_id, accepted.attempt_id);
        added.extend(
            bounded(
                limit,
                results::verify(&artifact, &receipt, &prefix, store, baseline.metadata()),
            )
            .await?,
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
    let input = store.file_io().new_input(snapshot.manifest_list())?;
    let bytes = bounded(limit, input.read()).await?;
    let list = ManifestList::parse_with_version(&bytes, FormatVersion::V2)?;
    let mut removed = Vec::new();
    for manifest in list.entries() {
        let loaded = bounded(limit, manifest.load_manifest(store.file_io())).await?;
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
    let proposal = RewriteProposal {
        job_id,
        publication_id: PublicationId::generate(),
        snapshot_id: new_snapshot_id(baseline.metadata()),
        removed,
        added,
    };
    bounded(limit, proposal.prepare(baseline)).await
}

/// Records a publication decision. Audit failure is logged and never changes
/// the outcome already decided from durable evidence.
async fn record_publication_decision(
    conn: &mut PgConnection,
    job: &Job,
    action: bergsmith_common::audit::AuditAction,
    outcome: bergsmith_common::audit::AuditOutcome,
    reason: &str,
) {
    if let Err(error) = crate::identity::audit::job_decision(
        conn,
        job.organization_id,
        Some(job.created_by),
        action,
        &job.id.to_string(),
        outcome,
        Some(reason),
    )
    .await
    {
        tracing::warn!(
            reason = error.reason_code(),
            job_id = %job.id,
            "publication audit record failed"
        );
    }
}
