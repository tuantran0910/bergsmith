//! Catalog and storage access for the serve runtime.
//!
//! A job names a canonical table, not a catalog. The runtime resolves one
//! alias the job's requester may use, keeps the catalog client per unchanged
//! connection, and selects the storage profile by the table's location. No
//! credential is persisted or logged.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use bergsmith_catalog::{CatalogConnection, ConnectionConfig, LoadedTable, TableName};
use bergsmith_common::ids::{CatalogConnectionId, JobId, OrganizationId, TableId};
use bergsmith_common::permission::Permission;
use bergsmith_persistence::tenancy::CatalogConnection as StoredConnection;
use bergsmith_persistence::{Database, PersistenceError, jobs, tables};
use bergsmith_storage::{
    CredentialCell, Location, ObjectStore, ProfileError, StorageError, StorageProfile,
};
use futures::StreamExt as _;
use tokio::sync::Mutex;
use tonic::Status;
use url::Url;
use uuid::Uuid;

use crate::durable_publication;
use crate::runtime::config::RuntimeProfiles;
use crate::scheduling::Eligibility;
use crate::workers::{TaskAccess, WorkerAccess};
use bergsmith_persistence::jobs::Job;

const VALIDATION_CONCURRENCY: usize = 16;

/// A resolved table with the catalog and storage that serve it.
pub struct TableBinding {
    /// Catalog name used to load the table.
    pub table: TableName,
    /// Persisted connection settings.
    pub connection: StoredConnection,
    /// Authenticated catalog client.
    pub catalog: Arc<CatalogConnection>,
    /// Profile selected by the table location.
    pub profile: StorageProfile,
    /// Storage opened with vended credentials, when the backend needs them.
    pub store: ObjectStore,
    /// The table as loaded, including the location used for commits.
    pub loaded: LoadedTable,
    /// Where this job may write attempt outputs.
    pub data_location: Location,
}

/// Why the runtime could not resolve a table.
#[derive(Debug, thiserror::Error)]
pub enum AccessError {
    /// The ledger read or write failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// The catalog call failed.
    #[error(transparent)]
    Catalog(#[from] bergsmith_catalog::CatalogError),
    /// No configured storage profile serves the table location.
    #[error(transparent)]
    Profile(#[from] ProfileError),
    /// Storage could not be opened.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// The publication intent could not be read.
    #[error(transparent)]
    Publication(#[from] durable_publication::DurablePublicationError),
    /// The requester no longer has a usable alias for the table.
    #[error("no accessible catalog alias for table {0}")]
    NoAlias(TableId),
    /// The connection names an external credential the runtime does not resolve.
    #[error("catalog connection {0} names an unresolvable credential reference")]
    CredentialUnavailable(CatalogConnectionId),
    /// The stored connection settings cannot be used as they are.
    #[error("catalog connection {0} is misconfigured: {1}")]
    InvalidConnection(CatalogConnectionId, &'static str),
    /// The catalog returned a different table than the canonical identity.
    #[error("table {table} is {observed}, not the canonical {expected}")]
    IdentityChanged {
        /// Canonical table.
        table: TableId,
        /// Expected `table-uuid`.
        expected: Uuid,
        /// Observed `table-uuid`.
        observed: Uuid,
    },
}

impl AccessError {
    /// Whether retrying the same operation can succeed without an operator.
    #[must_use]
    pub const fn is_transient(&self) -> bool {
        match self {
            Self::Persistence(_) => true,
            Self::Publication(error) => matches!(
                error,
                durable_publication::DurablePublicationError::Persistence(
                    PersistenceError::Database(_) | PersistenceError::LeadershipLost { .. }
                ) | durable_publication::DurablePublicationError::Catalog(_)
                    | durable_publication::DurablePublicationError::Timeout
            ),
            Self::Catalog(error) => !matches!(
                error,
                bergsmith_catalog::CatalogError::Status { status: 403, .. }
                    | bergsmith_catalog::CatalogError::Status { status: 404, .. }
                    | bergsmith_catalog::CatalogError::MissingEndpoints(_)
                    | bergsmith_catalog::CatalogError::InvalidUrl(_)
                    | bergsmith_catalog::CatalogError::InvalidLocation(_)
                    | bergsmith_catalog::CatalogError::DataLocationOutsideTable(_)
            ),
            Self::NoAlias(_)
            | Self::IdentityChanged { .. }
            | Self::Profile(_)
            | Self::CredentialUnavailable(_)
            | Self::InvalidConnection(_, _) => false,
            // A storage read can recover on its own; a malformed listing or an
            // oversized one cannot.
            Self::Storage(error) => matches!(error, StorageError::Io(_)),
        }
    }

    /// A stable reason for job state and operator logs.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Persistence(_) => "ExecutionLedgerUnavailable",
            Self::Publication(_) => "PublicationIntentUnavailable",
            Self::Catalog(_) => "CatalogUnavailable",
            Self::Profile(_) => "StorageProfileUnavailable",
            Self::Storage(_) => "StorageUnavailable",
            Self::NoAlias(_) => "AuthorizationRevoked",
            Self::CredentialUnavailable(_) => "CatalogCredentialUnavailable",
            Self::InvalidConnection(_, _) => "CatalogConnectionInvalid",
            Self::IdentityChanged { .. } => "TableIdentityChanged",
        }
    }
}

/// The catalog clients and storage profiles used by every runtime loop.
pub struct RuntimeCatalogs {
    database: Database,
    profiles: RuntimeProfiles,
    catalogs: Mutex<BTreeMap<CatalogConnectionId, CachedCatalog>>,
}

struct CachedCatalog {
    uri: String,
    warehouse: Option<String>,
    catalog: Arc<CatalogConnection>,
}

impl RuntimeCatalogs {
    /// Creates access from the runtime database and profile set.
    #[must_use]
    pub fn new(database: Database, profiles: RuntimeProfiles) -> Self {
        Self {
            database,
            profiles,
            catalogs: Mutex::new(BTreeMap::new()),
        }
    }

    /// The database handle used by the worker access adapter.
    #[must_use]
    pub const fn database(&self) -> &Database {
        &self.database
    }

    /// Resolves the table, catalog, and storage for `job`.
    ///
    /// # Errors
    ///
    /// [`AccessError`] when the requester has no usable alias, the catalog or
    /// storage settings are unusable, or the catalog changed the table UUID.
    pub async fn binding_for_job(&self, job: &Job) -> Result<TableBinding, AccessError> {
        let mut conn = self
            .database
            .pool()
            .acquire()
            .await
            .map_err(PersistenceError::from)?;
        let identity = tables::table_identity(&mut conn, job.organization_id, job.table_id).await?;
        let roles: Vec<&str> = Permission::SubmitJobs
            .roles()
            .iter()
            .map(|role| role.as_str())
            .collect();
        let aliases = tables::accessible_aliases_with_connections(
            &mut conn,
            job.organization_id,
            job.created_by,
            &roles,
            job.table_id,
        )
        .await?;
        // Prefer a route that may run maintenance for planning and new work
        // alike, then an enabled route, and fall back to a disabled or paused
        // route when existing work still needs the connection.
        let Some(target) = aliases
            .into_iter()
            .min_by_key(|alias| route_rank(&alias.connection))
        else {
            return Err(AccessError::NoAlias(job.table_id));
        };
        drop(conn);
        self.binding_for_alias(
            job.table_id,
            identity.table_uuid,
            TableName {
                namespace: target.alias.namespace,
                name: target.alias.name,
            },
            target.connection,
        )
        .await
    }

    /// Resolves a table through an explicit stored connection, for publication
    /// recovery where the immutable intent names the connection.
    ///
    /// # Errors
    ///
    /// [`AccessError`] as [`Self::binding_for_job`].
    pub async fn binding_for_connection(
        &self,
        organization_id: OrganizationId,
        connection_id: CatalogConnectionId,
        table_id: TableId,
        table_uuid: Uuid,
        table: TableName,
    ) -> Result<TableBinding, AccessError> {
        let mut conn = self
            .database
            .pool()
            .acquire()
            .await
            .map_err(PersistenceError::from)?;
        let connection = bergsmith_persistence::tenancy::catalog_connection(
            &mut conn,
            organization_id,
            connection_id,
        )
        .await?;
        drop(conn);
        self.binding_for_alias(table_id, table_uuid, table, connection)
            .await
    }

    /// Opens the catalog for a stored connection without loading a table.
    ///
    /// # Errors
    ///
    /// [`AccessError`] for a rejected credential reference or unusable URI.
    pub async fn catalog_for(
        &self,
        connection: &StoredConnection,
    ) -> Result<Arc<CatalogConnection>, AccessError> {
        let config = connection_config(connection)?;
        self.catalog(&config, connection).await
    }

    /// Reads the persisted connection named by a publication intent.
    ///
    /// Recovery must not depend on a mutable alias or storage profile; the
    /// immutable intent names the connection whose metadata proves or refutes
    /// publication.
    ///
    /// # Errors
    ///
    /// [`AccessError::Persistence`] for a missing intent or connection.
    pub async fn intent_connection(
        &self,
        conn: &mut sqlx::PgConnection,
        job: &Job,
    ) -> Result<StoredConnection, AccessError> {
        let connection_id = durable_publication::publication_connection(conn, job.id).await?;
        Ok(bergsmith_persistence::tenancy::catalog_connection(
            conn,
            job.organization_id,
            connection_id,
        )
        .await?)
    }

    async fn binding_for_alias(
        &self,
        table_id: TableId,
        table_uuid: Uuid,
        name: TableName,
        connection: StoredConnection,
    ) -> Result<TableBinding, AccessError> {
        let catalog = self.catalog_for(&connection).await?;
        let mut loaded = catalog.load_table(&name, false).await?;
        verify_identity(table_id, table_uuid, loaded.metadata.uuid())?;
        let profiles = self.profiles.for_connection(connection.id);
        let mut profile = profiles.for_location(&loaded.location)?.clone();
        // S3 reads need table-scoped credentials; local/memory do not.
        if matches!(profile, StorageProfile::S3(_)) {
            loaded = catalog.load_table(&name, true).await?;
            verify_identity(table_id, table_uuid, loaded.metadata.uuid())?;
            profile = profiles.for_location(&loaded.location)?.clone();
        }
        let store = open_store(&profile, &loaded)?;
        let data_location = loaded.data_location()?;
        Ok(TableBinding {
            table: name,
            connection,
            catalog,
            profile,
            store,
            loaded,
            data_location,
        })
    }

    async fn catalog(
        &self,
        config: &ConnectionConfig,
        connection: &StoredConnection,
    ) -> Result<Arc<CatalogConnection>, AccessError> {
        {
            let entries = self.catalogs.lock().await;
            if let Some(entry) = entries.get(&connection.id)
                && entry.matches(connection)
            {
                return Ok(entry.catalog.clone());
            }
        }
        let catalog = Arc::new(CatalogConnection::connect(config).await?);
        let mut entries = self.catalogs.lock().await;
        match entries.get_mut(&connection.id) {
            Some(entry) if entry.matches(connection) => return Ok(entry.catalog.clone()),
            Some(entry) => {
                *entry = CachedCatalog {
                    uri: connection.uri.clone(),
                    warehouse: connection.warehouse.clone(),
                    catalog: catalog.clone(),
                };
            }
            None => {
                entries.insert(
                    connection.id,
                    CachedCatalog {
                        uri: connection.uri.clone(),
                        warehouse: connection.warehouse.clone(),
                        catalog: catalog.clone(),
                    },
                );
            }
        }
        Ok(catalog)
    }
}

impl CachedCatalog {
    fn matches(&self, connection: &StoredConnection) -> bool {
        self.uri == connection.uri && self.warehouse == connection.warehouse
    }
}

/// How usable a connection is for maintenance, best first.
///
/// A connection that may run maintenance and is in service ranks first, then
/// one that is in service but paused, then one that is out of service.
fn route_rank(connection: &StoredConnection) -> u8 {
    match (
        connection.enabled,
        connection.settings.maintenance_enabled(),
    ) {
        (true, true) => 0,
        (true, false) => 1,
        (false, _) => 2,
    }
}

/// Builds connection settings from a stored connection.
///
/// # Errors
///
/// [`AccessError::CredentialUnavailable`] for an external credential reference,
/// and [`AccessError::InvalidConnection`] for a malformed URI.
pub fn connection_config(connection: &StoredConnection) -> Result<ConnectionConfig, AccessError> {
    if connection.credential_ref.is_some() {
        return Err(AccessError::CredentialUnavailable(connection.id));
    }
    let uri = Url::parse(&connection.uri).map_err(|_| {
        AccessError::InvalidConnection(connection.id, "the catalog URI is not a URL")
    })?;
    let mut config = ConnectionConfig::new(uri);
    config.warehouse.clone_from(&connection.warehouse);
    Ok(config)
}

fn verify_identity(table: TableId, expected: Uuid, observed: Uuid) -> Result<(), AccessError> {
    if expected == observed {
        Ok(())
    } else {
        Err(AccessError::IdentityChanged {
            table,
            expected,
            observed,
        })
    }
}

fn open_store(profile: &StorageProfile, loaded: &LoadedTable) -> Result<ObjectStore, AccessError> {
    let credentials = matches!(profile, StorageProfile::S3(_))
        .then(|| CredentialCell::new(loaded.credentials.clone()));
    Ok(ObjectStore::open(profile, credentials)?)
}

/// Worker table access backed by the runtime catalog resolver.
///
/// Every call reloads the table and vends current credentials; nothing durable
/// is held in memory except the catalog client.
pub struct RuntimeWorkerAccess {
    catalogs: Arc<RuntimeCatalogs>,
}

impl RuntimeWorkerAccess {
    /// Creates an access provider.
    #[must_use]
    pub const fn new(catalogs: Arc<RuntimeCatalogs>) -> Self {
        Self { catalogs }
    }
}

#[async_trait]
impl WorkerAccess for RuntimeWorkerAccess {
    async fn access(&self, job: JobId) -> Result<TaskAccess, Status> {
        let job = {
            let mut conn = self
                .catalogs
                .database()
                .pool()
                .acquire()
                .await
                .map_err(|_| Status::unavailable("worker ledger unavailable"))?;
            job_record(&mut conn, job).await?
        };
        let binding = self
            .catalogs
            .binding_for_job(&job)
            .await
            .map_err(|error| worker_status(&error))?;
        let vend = matches!(binding.profile, StorageProfile::S3(_));
        // Reload with delegation so the grant sees the freshest credentials.
        let loaded = binding
            .catalog
            .load_table(&binding.table, vend)
            .await
            .map_err(|_| Status::unavailable("worker catalog access unavailable"))?;
        let store = open_store(&binding.profile, &loaded)
            .map_err(|_| Status::unavailable("worker object store unavailable"))?;
        let credentials = if vend {
            let cell = CredentialCell::new(loaded.credentials);
            Some(
                cell.select(&loaded.location, std::time::Duration::from_secs(30))
                    .map_err(|_| Status::unavailable("worker table credential unavailable"))?,
            )
        } else {
            None
        };
        let wire_credentials = credentials.as_ref().map(credential_to_wire);
        Ok(TaskAccess {
            store,
            metadata: loaded.metadata,
            credentials: wire_credentials,
        })
    }
}

#[async_trait]
impl crate::scheduling::SchedulingAccess for RuntimeWorkerAccess {
    async fn validate(
        &self,
        job: &Job,
        plan: &bergsmith_maintenance::PlanDocument,
    ) -> Result<Eligibility, Status> {
        let binding = match self.catalogs.binding_for_job(job).await {
            Ok(binding) => binding,
            Err(error) => {
                if let Some(eligibility) = initial_eligibility(&error) {
                    return Ok(eligibility);
                }
                return Err(worker_status(&error));
            }
        };
        let vend = matches!(binding.profile, StorageProfile::S3(_));
        let loaded = match binding.catalog.load_table(&binding.table, vend).await {
            Ok(loaded) => loaded,
            Err(error) if error.is_not_found() => {
                return Ok(Eligibility::ReplanRequired("PlanningTableMissing"));
            }
            Err(bergsmith_catalog::CatalogError::Status { status: 403, .. }) => {
                return Ok(Eligibility::PermanentFailure("ExecutionAuthorityRevoked"));
            }
            Err(
                bergsmith_catalog::CatalogError::Status {
                    status: 401 | 419, ..
                }
                | bergsmith_catalog::CatalogError::Authentication { .. },
            ) => {
                return Err(Status::unavailable(
                    "scheduler catalog authentication rejected",
                ));
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
        let store = open_store(&binding.profile, &loaded)
            .map_err(|_| Status::unavailable("scheduler object store unavailable"))?;
        let metadata = Location::parse(plan.baseline().metadata_location())
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
        let manifest = Location::parse(snapshot.manifest_list())
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
        let files: Vec<_> = files
            .into_iter()
            .map(|(path, size)| (path.to_owned(), size))
            .collect();
        let mut checks = futures::stream::iter(files)
            .map(|(path, size)| {
                let store = store.clone();
                async move { validate_source(&store, &path, size).await }
            })
            .buffer_unordered(VALIDATION_CONCURRENCY);
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
        job: &Job,
        epoch: i64,
    ) -> Result<(), Status> {
        let connection = {
            let mut conn = pool
                .acquire()
                .await
                .map_err(|_| Status::unavailable("publication ledger unavailable"))?;
            self.catalogs
                .intent_connection(&mut conn, job)
                .await
                .map_err(|error| worker_status(&error))?
        };
        // Metadata-only recovery needs the catalog, not storage credentials or
        // a mutable alias. A vending outage must not hide matching evidence.
        let catalog = self
            .catalogs
            .catalog_for(&connection)
            .await
            .map_err(|error| worker_status(&error))?;
        let mut conn = pool
            .acquire()
            .await
            .map_err(|_| Status::unavailable("publication ledger unavailable"))?;
        durable_publication::reconcile(&mut conn, job, epoch, &catalog)
            .await
            .map(|_| ())
            .map_err(|_| Status::unavailable("publication recovery unavailable"))
    }
}

async fn job_record(conn: &mut sqlx::PgConnection, job: JobId) -> Result<Job, Status> {
    jobs::job_by_id(conn, job)
        .await
        .map_err(|_| Status::unavailable("worker ledger unavailable"))
}

/// A terminal eligibility decided by the first catalog resolution.
///
/// Without this classification a persistent 404, 403, identity change, or
/// unusable connection would be reported as a transient worker status and
/// reconciliation would defer the job forever.
fn initial_eligibility(error: &AccessError) -> Option<Eligibility> {
    match error {
        AccessError::Catalog(bergsmith_catalog::CatalogError::Status { status: 404, .. }) => {
            Some(Eligibility::ReplanRequired("PlanningTableMissing"))
        }
        AccessError::Catalog(bergsmith_catalog::CatalogError::Status { status: 403, .. }) => {
            Some(Eligibility::PermanentFailure("ExecutionAuthorityRevoked"))
        }
        AccessError::IdentityChanged { .. } => {
            Some(Eligibility::PermanentFailure("TableIdentityChanged"))
        }
        AccessError::NoAlias(_) => Some(Eligibility::PermanentFailure("AuthorizationRevoked")),
        AccessError::CredentialUnavailable(_) | AccessError::InvalidConnection(_, _) => Some(
            Eligibility::PermanentFailure("CatalogConfigurationUnavailable"),
        ),
        AccessError::Profile(_) | AccessError::Storage(_) => Some(Eligibility::PermanentFailure(
            "StorageConfigurationUnavailable",
        )),
        AccessError::Persistence(_) | AccessError::Publication(_) | AccessError::Catalog(_) => None,
    }
}

fn worker_status(error: &AccessError) -> Status {
    match error {
        AccessError::Persistence(_) => Status::unavailable("worker ledger unavailable"),
        AccessError::Publication(_) => Status::unavailable("publication intent unavailable"),
        AccessError::NoAlias(_) => Status::permission_denied("worker job authorization revoked"),
        AccessError::IdentityChanged { .. } => {
            Status::failed_precondition("worker table identity changed")
        }
        AccessError::CredentialUnavailable(_) | AccessError::InvalidConnection(_, _) => {
            Status::failed_precondition("worker catalog configuration unavailable")
        }
        AccessError::Profile(_) | AccessError::Storage(_) => {
            Status::unavailable("worker storage unavailable")
        }
        AccessError::Catalog(_) => Status::unavailable("worker catalog unavailable"),
    }
}

fn credential_to_wire(
    credential: &bergsmith_storage::StorageCredential,
) -> bergsmith_protocol::cluster::StorageCredentials {
    use std::time::UNIX_EPOCH;
    let bergsmith_storage::CredentialKind::S3 {
        access_key_id,
        secret_access_key,
        session_token,
    } = &credential.kind;
    let mut properties: BTreeMap<String, String> = BTreeMap::from([
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
    bergsmith_protocol::cluster::StorageCredentials {
        prefix: credential.prefix.to_string(),
        properties,
        expires_at,
    }
}

async fn validate_source(
    store: &ObjectStore,
    path: &str,
    expected_size: u64,
) -> Result<Eligibility, Status> {
    let location = Location::parse(path)
        .map_err(|_| Status::failed_precondition("invalid planned input location"))?;
    let input = store
        .file_io()
        .new_input(path)
        .map_err(|_| Status::unavailable("scheduler input metadata unavailable"))?;
    let Ok(actual) = input.metadata().await else {
        // FileIO erases the backend's typed not-found error. Check existence
        // only on failure, so healthy files need a single metadata request.
        if !store
            .exists(&location)
            .await
            .map_err(|_| Status::unavailable("scheduler input check unavailable"))?
        {
            return Ok(Eligibility::PermanentFailure("PlannedSourceMissing"));
        }
        return Err(Status::unavailable("scheduler input metadata unavailable"));
    };
    Ok(if actual.size == expected_size {
        Eligibility::Valid
    } else {
        Eligibility::PermanentFailure("PlannedSourceIntegrityFailure")
    })
}

async fn validate_manifests(
    store: &ObjectStore,
    manifest: &Location,
) -> Result<Eligibility, Status> {
    let bytes = store
        .read(manifest)
        .await
        .map_err(|_| Status::unavailable("scheduler manifest list unavailable"))?;
    let Ok(manifests) =
        iceberg::spec::ManifestList::parse_with_version(&bytes, iceberg::spec::FormatVersion::V2)
    else {
        return Ok(Eligibility::PermanentFailure("PlanningMetadataCorrupt"));
    };
    let paths = manifests
        .entries()
        .iter()
        .map(|entry| {
            Location::parse(&entry.manifest_path)
                .map_err(|_| Status::failed_precondition("invalid manifest location"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    validate_retained_paths(store, paths).await
}

async fn validate_retained_paths(
    store: &ObjectStore,
    paths: Vec<Location>,
) -> Result<Eligibility, Status> {
    let mut checks = futures::stream::iter(paths)
        .map(|path| async move {
            store
                .exists(&path)
                .await
                .map_err(|_| Status::unavailable("scheduler retention check unavailable"))
        })
        .buffer_unordered(VALIDATION_CONCURRENCY);
    while let Some(exists) = checks.next().await {
        if !exists? {
            return Ok(Eligibility::ReplanRequired("PlanningMetadataNotRetained"));
        }
    }
    Ok(Eligibility::Valid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(code: u16) -> AccessError {
        AccessError::Catalog(bergsmith_catalog::CatalogError::Status {
            operation: "test",
            status: code,
            model: None,
        })
    }

    #[test]
    fn initial_resolution_classifies_terminal_failures() {
        assert_eq!(
            initial_eligibility(&status(404)),
            Some(Eligibility::ReplanRequired("PlanningTableMissing"))
        );
        assert_eq!(
            initial_eligibility(&status(403)),
            Some(Eligibility::PermanentFailure("ExecutionAuthorityRevoked"))
        );
        assert_eq!(
            initial_eligibility(&AccessError::NoAlias(TableId::generate())),
            Some(Eligibility::PermanentFailure("AuthorizationRevoked"))
        );
        assert_eq!(
            initial_eligibility(&AccessError::IdentityChanged {
                table: TableId::generate(),
                expected: Uuid::now_v7(),
                observed: Uuid::now_v7(),
            }),
            Some(Eligibility::PermanentFailure("TableIdentityChanged"))
        );
        assert_eq!(initial_eligibility(&status(401)), None);
        assert_eq!(initial_eligibility(&status(500)), None);
    }
    fn s3_store(endpoint: String) -> ObjectStore {
        let credential = bergsmith_storage::StorageCredential::from_vended(
            "s3://warehouse",
            &BTreeMap::from([
                ("s3.access-key-id".into(), "development".into()),
                ("s3.secret-access-key".into(), "development-only".into()),
            ]),
        )
        .unwrap();
        ObjectStore::open(
            &StorageProfile::S3(bergsmith_storage::S3Profile {
                endpoint: Some(endpoint),
                region: "us-east-1".into(),
                path_style_access: true,
            }),
            Some(CredentialCell::new(vec![credential])),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_healthy_source_needs_one_metadata_request() {
        let mut server = mockito::Server::new_async().await;
        let head = server
            .mock("HEAD", "/warehouse/source")
            .with_status(200)
            .with_header("content-length", "123")
            .expect(1)
            .create_async()
            .await;
        let store = s3_store(server.url());
        assert_eq!(
            validate_source(&store, "s3://warehouse/source", 123)
                .await
                .unwrap(),
            Eligibility::Valid
        );
        head.assert_async().await;
    }

    #[tokio::test]
    async fn missing_and_changed_sources_keep_their_terminal_classification() {
        let mut server = mockito::Server::new_async().await;
        let missing = server
            .mock("HEAD", "/warehouse/missing")
            .with_status(404)
            .expect(2)
            .create_async()
            .await;
        let changed = server
            .mock("HEAD", "/warehouse/changed")
            .with_status(200)
            .with_header("content-length", "9")
            .expect(1)
            .create_async()
            .await;
        let store = s3_store(server.url());
        assert_eq!(
            validate_source(&store, "s3://warehouse/missing", 123)
                .await
                .unwrap(),
            Eligibility::PermanentFailure("PlannedSourceMissing")
        );
        assert_eq!(
            validate_source(&store, "s3://warehouse/changed", 123)
                .await
                .unwrap(),
            Eligibility::PermanentFailure("PlannedSourceIntegrityFailure")
        );
        missing.assert_async().await;
        changed.assert_async().await;
    }

    #[tokio::test]
    async fn manifest_retention_checks_run_in_bounded_parallel_batches() {
        let barrier = Arc::new(tokio::sync::Barrier::new(VALIDATION_CONCURRENCY));
        let app = axum::Router::new().route(
            "/{*path}",
            axum::routing::head({
                let barrier = barrier.clone();
                move || {
                    let barrier = barrier.clone();
                    async move {
                        barrier.wait().await;
                        axum::http::StatusCode::OK
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let store = s3_store(endpoint);
        let paths = (0..VALIDATION_CONCURRENCY * 2)
            .map(|index| Location::parse(&format!("s3://warehouse/manifest-{index}")).unwrap())
            .collect();
        // Every response waits for a whole concurrent batch; a serial checker
        // cannot complete. The timeout only bounds a broken implementation.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            validate_retained_paths(&store, paths),
        )
        .await;
        server.abort();
        assert_eq!(result.unwrap().unwrap(), Eligibility::Valid);
    }

    #[tokio::test]
    async fn missing_retained_metadata_requires_a_new_plan() {
        let directory = tempfile::tempdir().unwrap();
        let store = ObjectStore::open(&StorageProfile::LocalFs, None).unwrap();
        let missing = Location::parse(&format!(
            "file://{}/missing.avro",
            directory.path().display()
        ))
        .unwrap();
        assert_eq!(
            validate_retained_paths(&store, vec![missing])
                .await
                .unwrap(),
            Eligibility::ReplanRequired("PlanningMetadataNotRetained")
        );
    }
}
