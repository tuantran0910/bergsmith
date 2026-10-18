//! Catalog-backed, memory-only worker storage access.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use bergsmith_catalog::{CatalogConnection, TableName};
use bergsmith_common::ids::JobId;
use bergsmith_protocol::cluster::StorageCredentials;
use bergsmith_storage::{CredentialCell, CredentialKind, ObjectStore, StorageProfile};
use futures::StreamExt;
use tonic::Status;

use crate::workers::{TaskAccess, WorkerAccess};

/// Catalog authority and storage configuration selected for a job by admission.
pub struct WorkerTableBinding {
    /// Authenticated coordinator catalog client, never passed to a worker.
    pub catalog: Arc<CatalogConnection>,
    /// Original catalog table name.
    pub table: TableName,
    /// Backend configuration without ambient credentials.
    pub profile: StorageProfile,
}

/// Resolves admitted job bindings and vends fresh credentials for every call.
/// The admission/recovery caller reconstructs bindings from durable job identity.
pub struct CatalogWorkerAccess {
    bindings: BTreeMap<JobId, WorkerTableBinding>,
}

impl CatalogWorkerAccess {
    /// Creates a provider for the explicitly admitted jobs.
    #[must_use]
    pub fn new(bindings: BTreeMap<JobId, WorkerTableBinding>) -> Self {
        Self { bindings }
    }
}

#[async_trait]
impl WorkerAccess for CatalogWorkerAccess {
    async fn access(&self, job: JobId) -> Result<TaskAccess, Status> {
        let binding = self
            .bindings
            .get(&job)
            .ok_or_else(|| Status::failed_precondition("missing worker table binding"))?;
        let vend = matches!(binding.profile, StorageProfile::S3(_));
        let loaded = binding
            .catalog
            .load_table(&binding.table, vend)
            .await
            .map_err(|_| Status::unavailable("worker catalog access unavailable"))?;
        let cell = CredentialCell::new(loaded.credentials);
        let credentials = if vend {
            let credential = cell
                .select(&loaded.location, Duration::from_secs(30))
                .map_err(|_| Status::unavailable("worker table credential unavailable"))?;
            let CredentialKind::S3 {
                access_key_id,
                secret_access_key,
                session_token,
            } = credential.kind;
            let mut properties = BTreeMap::from([
                ("s3.access-key-id".into(), access_key_id.expose().to_owned()),
                (
                    "s3.secret-access-key".into(),
                    secret_access_key.expose().to_owned(),
                ),
            ]);
            if let Some(token) = session_token {
                properties.insert("s3.session-token".into(), token.expose().to_owned());
            }
            let expires_at = credential
                .expires_at
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .and_then(|value| {
                    i64::try_from(value.as_secs())
                        .ok()
                        .map(|seconds| prost_types::Timestamp {
                            seconds,
                            nanos: value.subsec_nanos().cast_signed(),
                        })
                });
            Some(StorageCredentials {
                prefix: credential.prefix.to_string(),
                properties,
                expires_at,
            })
        } else {
            None
        };
        let store = ObjectStore::open(&binding.profile, vend.then_some(cell))
            .map_err(|_| Status::unavailable("worker object store unavailable"))?;
        Ok(TaskAccess {
            store,
            metadata: loaded.metadata,
            credentials,
        })
    }
}

#[async_trait]
impl crate::scheduling::SchedulingAccess for CatalogWorkerAccess {
    async fn validate(
        &self,
        job: &bergsmith_persistence::jobs::Job,
        plan: &bergsmith_maintenance::PlanDocument,
    ) -> Result<crate::scheduling::Eligibility, Status> {
        use crate::scheduling::Eligibility;
        let binding = self
            .bindings
            .get(&job.id)
            .ok_or_else(|| Status::unavailable("missing scheduler table binding"))?;
        let vend = matches!(binding.profile, StorageProfile::S3(_));
        let loaded = match binding.catalog.load_table(&binding.table, vend).await {
            Ok(loaded) => loaded,
            Err(error) if error.is_not_found() => {
                return Ok(Eligibility::ReplanRequired("PlanningTableMissing"));
            }
            Err(
                bergsmith_catalog::CatalogError::Status {
                    status: 401 | 403, ..
                }
                | bergsmith_catalog::CatalogError::Authentication { .. },
            ) => {
                return Ok(Eligibility::PermanentFailure("ExecutionAuthorityRevoked"));
            }
            Err(_) => return Err(Status::unavailable("scheduler catalog access unavailable")),
        };
        let captured = bergsmith_maintenance::PlanningBaseline::capture(
            &loaded.metadata,
            loaded.metadata_location.as_deref(),
        )
        .map_err(|_| Status::unavailable("scheduler table metadata unavailable"))?;
        if captured.as_ref() != Some(plan.baseline()) {
            return Ok(Eligibility::ReplanRequired("PlanningBaselineMismatch"));
        }
        let store = ObjectStore::open(
            &binding.profile,
            vend.then(|| CredentialCell::new(loaded.credentials)),
        )
        .map_err(|_| Status::unavailable("scheduler object store unavailable"))?;
        let metadata = bergsmith_storage::Location::parse(plan.baseline().metadata_location())
            .map_err(|_| Status::failed_precondition("invalid planned metadata location"))?;
        if !store
            .exists(&metadata)
            .await
            .map_err(|_| Status::unavailable("scheduler retention check unavailable"))?
        {
            return Ok(Eligibility::ReplanRequired("PlanningMetadataNotRetained"));
        }
        let snapshot = loaded
            .metadata
            .snapshot_by_id(plan.baseline().snapshot_id())
            .ok_or_else(|| Status::failed_precondition("missing baseline snapshot"))?;
        let manifest = bergsmith_storage::Location::parse(snapshot.manifest_list())
            .map_err(|_| Status::failed_precondition("invalid manifest list location"))?;
        if !store
            .exists(&manifest)
            .await
            .map_err(|_| Status::unavailable("scheduler retention check unavailable"))?
        {
            return Ok(Eligibility::ReplanRequired("PlanningMetadataNotRetained"));
        }
        let mut files = BTreeMap::new();
        for task in plan.tasks() {
            for file in task.sources() {
                files.insert(file.path(), file.file_size_bytes());
            }
            for file in task.deletes() {
                files.insert(file.path(), file.file_size_bytes());
            }
        }
        // Only existence and object metadata are read; coordinator never reads rows.
        // Ordered bounded concurrency preserves deterministic failure selection
        // while avoiding one network round trip per input in sequence.
        let files: Vec<_> = files
            .into_iter()
            .map(|(path, size)| (path.to_owned(), size))
            .collect();
        let mut checks = futures::stream::iter(files)
            .map(|(path, size)| {
                let store = store.clone();
                async move { validate_source(&store, &path, size).await }
            })
            .buffered(16);
        while let Some(result) = checks.next().await {
            let result = result?;
            if result != Eligibility::Valid {
                return Ok(result);
            }
        }
        validate_manifests(&store, &manifest).await
    }

    async fn recover_publication(
        &self,
        pool: &sqlx::PgPool,
        job: &bergsmith_persistence::jobs::Job,
    ) -> Result<(), Status> {
        let binding = self
            .bindings
            .get(&job.id)
            .ok_or_else(|| Status::unavailable("missing publication recovery binding"))?;
        let mut conn = pool
            .acquire()
            .await
            .map_err(|_| Status::unavailable("publication ledger unavailable"))?;
        crate::durable_publication::reconcile(&mut conn, job, &binding.catalog)
            .await
            .map_err(|_| Status::unavailable("publication recovery unavailable"))?;
        Ok(())
    }
}

async fn validate_source(
    store: &ObjectStore,
    path: &str,
    expected_size: u64,
) -> Result<crate::scheduling::Eligibility, Status> {
    use crate::scheduling::Eligibility;
    let location = bergsmith_storage::Location::parse(path)
        .map_err(|_| Status::failed_precondition("invalid planned input location"))?;
    if !store
        .exists(&location)
        .await
        .map_err(|_| Status::unavailable("scheduler input check unavailable"))?
    {
        return Ok(Eligibility::PermanentFailure("PlannedSourceMissing"));
    }
    let input = store
        .file_io()
        .new_input(path)
        .map_err(|_| Status::unavailable("scheduler input metadata unavailable"))?;
    let actual = input
        .metadata()
        .await
        .map_err(|_| Status::unavailable("scheduler input metadata unavailable"))?;
    Ok(if actual.size == expected_size {
        Eligibility::Valid
    } else {
        Eligibility::PermanentFailure("PlannedSourceIntegrityFailure")
    })
}

async fn validate_manifests(
    store: &ObjectStore,
    manifest: &bergsmith_storage::Location,
) -> Result<crate::scheduling::Eligibility, Status> {
    use crate::scheduling::Eligibility;
    let bytes = store
        .read(manifest)
        .await
        .map_err(|_| Status::unavailable("scheduler manifest list unavailable"))?;
    let Ok(manifests) =
        iceberg::spec::ManifestList::parse_with_version(&bytes, iceberg::spec::FormatVersion::V2)
    else {
        return Ok(Eligibility::PermanentFailure("PlanningMetadataCorrupt"));
    };
    for entry in manifests.entries() {
        let path = bergsmith_storage::Location::parse(&entry.manifest_path)
            .map_err(|_| Status::failed_precondition("invalid manifest location"))?;
        if !store
            .exists(&path)
            .await
            .map_err(|_| Status::unavailable("scheduler retention check unavailable"))?
        {
            return Ok(Eligibility::ReplanRequired("PlanningMetadataNotRetained"));
        }
    }
    Ok(Eligibility::Valid)
}
