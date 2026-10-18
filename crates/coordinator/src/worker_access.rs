//! Catalog-backed, memory-only worker storage access.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use bergsmith_catalog::{CatalogConnection, TableName};
use bergsmith_common::ids::JobId;
use bergsmith_protocol::cluster::StorageCredentials;
use bergsmith_storage::{CredentialCell, CredentialKind, ObjectStore, StorageProfile};
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
