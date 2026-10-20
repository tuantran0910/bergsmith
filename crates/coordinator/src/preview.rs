//! Catalog-backed table previews (BS-018.T2).
//!
//! A preview assesses one canonical table without creating a job, a plan, or
//! any durable work. It loads the current metadata from the table's catalog,
//! opens the storage that serves it, and asks the planner for an immutable
//! assessment. Nothing it reads is cached beyond the catalog client and
//! nothing it produces can publish.
//!
//! The catalog connection's authentication comes from configuration. A
//! connection that names an externally held credential is refused until the
//! runtime resolves credential references (ADR 0035); previews never fall back
//! to ambient identity.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use bergsmith_catalog::{CatalogConnection, CatalogError, ConnectionConfig, TableName};
use bergsmith_common::ids::{CatalogConnectionId, PrincipalId, TableId};
use bergsmith_maintenance::{PlanError, PlanInput, Planner, PlannerConfig, Preview};
use bergsmith_persistence::tables::TableAlias;
use bergsmith_persistence::tenancy::CatalogConnection as StoredConnection;
use bergsmith_storage::{CredentialCell, ObjectStore, StorageProfile, StorageProfiles};
use tokio::sync::{Mutex, Semaphore};
use url::Url;
use uuid::Uuid;

/// The most bins a preview returns. A table with very many partitions would
/// otherwise produce a response larger than any client can use.
pub const MAX_PREVIEW_BINS: usize = 1_000;

/// The most manifest entries one interactive preview reads.
///
/// This is deliberately stricter than the planner's own default: a preview is
/// a single HTTP request, while a planning job runs on its own budget.
pub const MAX_PREVIEW_MANIFEST_ENTRIES: u64 = 100_000;

/// The most previews one requester runs at once. A single operator cannot
/// consume every global permit and starve other tenants.
pub const MAX_CONCURRENT_PREVIEWS_PER_PRINCIPAL: usize = 2;

/// The most previews this process runs at once. Each one may read many
/// manifests and hold their metadata, so concurrency is bounded.
pub const MAX_CONCURRENT_PREVIEWS: usize = 8;

/// Bounds applied to previews.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewLimits {
    /// The most bins returned in one response.
    pub max_bins: usize,
    /// The most manifest entries read for one assessment.
    pub max_manifest_entries: u64,
    /// The most assessments this process runs at once.
    pub max_concurrent: usize,
    /// The most assessments one requester runs at once.
    pub max_concurrent_per_principal: usize,
}

impl Default for PreviewLimits {
    fn default() -> Self {
        Self {
            max_bins: MAX_PREVIEW_BINS,
            max_manifest_entries: MAX_PREVIEW_MANIFEST_ENTRIES,
            max_concurrent: MAX_CONCURRENT_PREVIEWS,
            max_concurrent_per_principal: MAX_CONCURRENT_PREVIEWS_PER_PRINCIPAL,
        }
    }
}

/// Storage profiles, per catalog connection.
///
/// A profile is configured per connection (ADR 0035): two S3-compatible
/// connections can have different endpoints, so selecting a profile by
/// backend alone would sign requests for the wrong store. Connections without
/// an entry use the fallback.
#[derive(Clone, Debug, Default)]
pub struct PreviewProfiles {
    by_connection: BTreeMap<CatalogConnectionId, StorageProfiles>,
    fallback: StorageProfiles,
}

impl PreviewProfiles {
    /// Profiles that apply to every connection without an entry of its own.
    #[must_use]
    pub fn new(fallback: StorageProfiles) -> Self {
        Self {
            by_connection: BTreeMap::new(),
            fallback,
        }
    }

    /// Adds the profiles of one connection, replacing any earlier entry.
    #[must_use]
    pub fn with_connection(
        mut self,
        connection: CatalogConnectionId,
        profiles: StorageProfiles,
    ) -> Self {
        self.by_connection.insert(connection, profiles);
        self
    }

    /// The profiles that apply to `connection`.
    #[must_use]
    pub fn for_connection(&self, connection: CatalogConnectionId) -> &StorageProfiles {
        self.by_connection
            .get(&connection)
            .unwrap_or(&self.fallback)
    }
}

/// Why a preview could not be produced.
///
/// A table the MVP cannot maintain is not an error: it produces a preview with
/// rejections. These errors mean the assessment itself could not finish.
#[derive(Debug, thiserror::Error)]
pub enum PreviewError {
    /// The catalog now reports a different `table-uuid` than the canonical
    /// identity, for example after the table was dropped and recreated.
    #[error("table {table} is {observed}, not the canonical {expected}")]
    IdentityChanged {
        /// The canonical table.
        table: TableId,
        /// The identity's `table-uuid`.
        expected: Uuid,
        /// The `table-uuid` the catalog returned.
        observed: Uuid,
    },
    /// The table's connection names a credential held outside PostgreSQL, and
    /// the runtime cannot resolve credential references yet.
    #[error("catalog connection {0} names an unresolvable credential reference")]
    CredentialUnavailable(CatalogConnectionId),
    /// The stored connection settings cannot be used as they are.
    #[error("catalog connection {0} is misconfigured: {1}")]
    ConnectionConfig(CatalogConnectionId, &'static str),
    /// No storage profile covers the table's location.
    #[error(transparent)]
    Profile(#[from] bergsmith_storage::ProfileError),
    /// Building or reading the table's storage failed.
    #[error(transparent)]
    Storage(#[from] bergsmith_storage::StorageError),
    /// The catalog could not be reached or refused the request.
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    /// Planning could not finish.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// Too many previews are already running.
    #[error("too many previews are already running")]
    Overloaded,
}

/// One table's catalog name and the connection that serves it.
///
/// Resolving this target is a ledger read (see
/// [`crate::api`]); it performs no catalog or object-store I/O. Callers
/// authorize the canonical table and choose a connection the caller may use
/// before [`CatalogPreviewer::preview_target`], so a denied caller or a
/// connection outside the caller's grants never reaches a catalog.
#[derive(Clone, PartialEq, Eq)]
pub struct PreviewTarget {
    /// The canonical table.
    pub table_id: TableId,
    /// The canonical `table-uuid` the loaded metadata must match.
    pub table_uuid: Uuid,
    /// The catalog name, in canonical alias order.
    pub alias: TableAlias,
    /// The connection the name belongs to.
    pub connection: StoredConnection,
}

/// An assessment and how many bins it held before the response bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssessedPreview {
    /// The immutable assessment. Its bins are truncated to the configured cap.
    pub preview: Preview,
    /// The number of bins before truncation.
    pub bins_total: usize,
}

impl PreviewError {
    /// A stable, sanitized kind for logs.
    ///
    /// This names the failure class only. It never includes the upstream
    /// catalog's message, a URL, a table location, or any other value that the
    /// coordinator does not control, so a deployment can log preview failures
    /// without forwarding catalog or Iceberg content.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        use bergsmith_catalog::CatalogError;
        match self {
            Self::IdentityChanged { .. } => "identity_changed",
            Self::CredentialUnavailable(_) => "credential_unavailable",
            Self::ConnectionConfig(..) => "connection_config",
            Self::Profile(_) => "profile",
            Self::Storage(_) => "storage",
            Self::Overloaded => "overloaded",
            Self::Plan(_) => "plan",
            Self::Catalog(error) => match error {
                CatalogError::InvalidUrl(_) => "catalog_invalid_url",
                CatalogError::Transport { .. } => "catalog_transport",
                CatalogError::Status { .. } => "catalog_status",
                CatalogError::Decode { .. } => "catalog_decode",
                CatalogError::Authentication { .. } => "catalog_authentication",
                CatalogError::MissingEndpoints(_) => "catalog_missing_endpoints",
                CatalogError::InvalidLocation(_) => "catalog_invalid_location",
                CatalogError::Credentials(_) => "catalog_credentials",
                CatalogError::NoVendedCredentials => "catalog_no_vended_credentials",
                CatalogError::DataLocationOutsideTable(_) => "catalog_data_location",
                CatalogError::IdempotencyUnsupported => "catalog_idempotency_unsupported",
            },
        }
    }

    /// The HTTP status an upstream catalog reported, when there was one.
    ///
    /// A status code is safe to log; the accompanying error body is not.
    #[must_use]
    pub const fn upstream_status(&self) -> Option<u16> {
        use bergsmith_catalog::CatalogError;
        match self {
            Self::Catalog(
                CatalogError::Status { status, .. } | CatalogError::Authentication { status },
            ) => Some(*status),
            _ => None,
        }
    }
}

/// Produces immutable previews of canonical tables.
#[derive(Clone)]
pub struct CatalogPreviewer {
    profiles: PreviewProfiles,
    planner: Planner,
    limits: PreviewLimits,
    capacity: Arc<Semaphore>,
    principals: Arc<Mutex<BTreeMap<PrincipalId, Arc<Semaphore>>>>,
    catalogs: Arc<CatalogCache>,
}

impl fmt::Debug for CatalogPreviewer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CatalogPreviewer")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// Catalog clients reused across previews, keyed by connection.
///
/// A client is reused only while the stored URI and warehouse are unchanged,
/// so editing a connection reconnects instead of using stale settings.
#[derive(Default)]
struct CatalogCache {
    entries: Mutex<BTreeMap<CatalogConnectionId, CachedCatalog>>,
}

struct CachedCatalog {
    uri: String,
    warehouse: Option<String>,
    catalog: Arc<CatalogConnection>,
}

impl CachedCatalog {
    fn matches(&self, connection: &StoredConnection) -> bool {
        self.uri == connection.uri && self.warehouse == connection.warehouse
    }
}

impl CatalogCache {
    async fn catalog(
        &self,
        config: &ConnectionConfig,
        connection: &StoredConnection,
    ) -> Result<Arc<CatalogConnection>, PreviewError> {
        {
            let entries = self.entries.lock().await;
            if let Some(entry) = entries.get(&connection.id)
                && entry.matches(connection)
            {
                return Ok(entry.catalog.clone());
            }
        }
        let catalog = Arc::new(CatalogConnection::connect(config).await?);
        let mut entries = self.entries.lock().await;
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

impl CatalogPreviewer {
    /// A previewer with the default limits.
    ///
    /// Pass the same [`PlannerConfig`] the planning path uses, so a preview
    /// never advertises limits that planning would not apply.
    ///
    /// # Errors
    ///
    /// [`PlanError::InvalidConfig`] when the planner limits are inconsistent.
    pub fn new(profiles: PreviewProfiles, planner: PlannerConfig) -> Result<Self, PlanError> {
        Self::with_limits(profiles, planner, PreviewLimits::default())
    }

    /// A previewer with explicit limits, for tests and constrained deployments.
    ///
    /// The effective manifest bound is the smaller of the planner's own bound
    /// and the preview bound, so a preview never reads more than either
    /// allows.
    ///
    /// # Errors
    ///
    /// [`PlanError::InvalidConfig`] when the planner limits are inconsistent.
    pub fn with_limits(
        profiles: PreviewProfiles,
        mut planner: PlannerConfig,
        limits: PreviewLimits,
    ) -> Result<Self, PlanError> {
        let limits = PreviewLimits {
            max_bins: limits.max_bins.max(1),
            max_manifest_entries: limits.max_manifest_entries.max(1),
            max_concurrent: limits.max_concurrent.max(1),
            max_concurrent_per_principal: limits.max_concurrent_per_principal.max(1),
        };
        planner.max_manifest_entries = planner
            .max_manifest_entries
            .min(limits.max_manifest_entries);
        let limits = PreviewLimits {
            max_manifest_entries: planner.max_manifest_entries,
            ..limits
        };
        Ok(Self {
            profiles,
            planner: Planner::new(planner)?,
            limits,
            capacity: Arc::new(Semaphore::new(limits.max_concurrent)),
            principals: Arc::default(),
            catalogs: Arc::default(),
        })
    }

    /// The planner configuration previews are assessed with.
    #[must_use]
    pub const fn planner(&self) -> &Planner {
        &self.planner
    }

    /// The limits this previewer applies.
    #[must_use]
    pub const fn limits(&self) -> PreviewLimits {
        self.limits
    }

    /// Assesses `target` without creating any work.
    ///
    /// `requester` is throttled independently, so one principal cannot consume
    /// every process-wide preview permit.
    ///
    /// # Errors
    ///
    /// A [`PreviewError`] when the connection cannot be configured, the catalog
    /// or storage is unavailable, the catalog's table identity differs from
    /// the canonical one, planning fails, or too many previews are running.
    /// Access to the table and the connection is the caller's responsibility;
    /// this function holds no grants and performs no authorization.
    pub async fn preview_target(
        &self,
        requester: PrincipalId,
        target: &PreviewTarget,
    ) -> Result<AssessedPreview, PreviewError> {
        // try_acquire is deliberately non-blocking: a queued preview would
        // still hold the request (and its body) while waiting for capacity.
        let _permit = self
            .capacity
            .try_acquire()
            .map_err(|_| PreviewError::Overloaded)?;
        let gate = {
            let mut principals = self.principals.lock().await;
            principals
                .entry(requester)
                .or_insert_with(|| {
                    Arc::new(Semaphore::new(self.limits.max_concurrent_per_principal))
                })
                .clone()
        };
        let _requester_permit = gate.try_acquire().map_err(|_| PreviewError::Overloaded)?;
        // Validate the stored connection (including credential references)
        // before the cache is consulted, so an edited connection is never
        // served from a client built under older settings.
        let config = connection_config(&target.connection)?;
        let catalog = self.catalogs.catalog(&config, &target.connection).await?;
        let name = TableName {
            namespace: target.alias.namespace.clone(),
            name: target.alias.name.clone(),
        };
        let profile = |location: &bergsmith_storage::Location| {
            self.profiles
                .for_connection(target.connection.id)
                .for_location(location)
                .cloned()
        };
        let mut loaded = catalog.load_table(&name, false).await?;
        // Identity is checked before storage selection or credential vending:
        // a replacement table must be reported as such even when its storage
        // configuration is unavailable.
        verify_identity(target, loaded.metadata.uuid())?;
        let mut storage = profile(&loaded.location)?;
        // The first load proves the table's location. Only S3 reads need the
        // catalog's table-scoped credentials, so only then is it loaded again.
        if matches!(storage, StorageProfile::S3(_)) {
            loaded = catalog.load_table(&name, true).await?;
            // The table could have been replaced between the two loads; check
            // again before using the delegated read.
            verify_identity(target, loaded.metadata.uuid())?;
            // The location could have been restated; reselect from the final
            // metadata so credentials and profile always agree.
            storage = profile(&loaded.location)?;
        }
        let store = ObjectStore::open(&storage, Some(CredentialCell::new(loaded.credentials)))?;
        let mut preview = self
            .planner
            .preview(PlanInput {
                metadata: &loaded.metadata,
                metadata_location: loaded.metadata_location.as_deref(),
                file_io: store.file_io(),
                selection: &[],
            })
            .await?;
        let bins_total = preview.bins.len();
        preview.bins.truncate(self.limits.max_bins);
        Ok(AssessedPreview {
            preview,
            bins_total,
        })
    }
}

/// Checks that the catalog returned the canonical table.
fn verify_identity(target: &PreviewTarget, observed: Uuid) -> Result<(), PreviewError> {
    if observed == target.table_uuid {
        Ok(())
    } else {
        Err(PreviewError::IdentityChanged {
            table: target.table_id,
            expected: target.table_uuid,
            observed,
        })
    }
}

/// Builds connection settings from a stored connection.
///
/// Only an unauthenticated connection is usable until the runtime resolves
/// credential references; an unresolvable reference is refused rather than
/// silently connecting without authentication.
fn connection_config(connection: &StoredConnection) -> Result<ConnectionConfig, PreviewError> {
    if connection.credential_ref.is_some() {
        return Err(PreviewError::CredentialUnavailable(connection.id));
    }
    let uri = Url::parse(&connection.uri).map_err(|_| {
        PreviewError::ConnectionConfig(connection.id, "the catalog URI is not a valid URL")
    })?;
    let mut config = ConnectionConfig::new(uri);
    config.warehouse.clone_from(&connection.warehouse);
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bergsmith_storage::Location;

    #[test]
    fn limits_clamp_to_the_planner_bound() {
        let previewer = CatalogPreviewer::with_limits(
            PreviewProfiles::default(),
            PlannerConfig {
                max_manifest_entries: 50,
                ..PlannerConfig::default()
            },
            PreviewLimits {
                max_manifest_entries: 10_000,
                ..PreviewLimits::default()
            },
        )
        .unwrap();
        assert_eq!(previewer.limits().max_manifest_entries, 50);
        assert_eq!(previewer.planner().config().max_manifest_entries, 50);

        let clamped = CatalogPreviewer::with_limits(
            PreviewProfiles::default(),
            PlannerConfig::default(),
            PreviewLimits {
                max_bins: 0,
                max_manifest_entries: 0,
                max_concurrent: 0,
                max_concurrent_per_principal: 0,
            },
        )
        .unwrap();
        assert_eq!(clamped.limits().max_bins, 1);
        assert_eq!(clamped.limits().max_concurrent, 1);
        assert_eq!(clamped.limits().max_concurrent_per_principal, 1);
        assert_eq!(clamped.limits().max_manifest_entries, 1);
    }

    #[test]
    fn profiles_are_selected_per_connection_with_a_fallback() {
        let connection = CatalogConnectionId::generate();
        let profiles = PreviewProfiles::new(StorageProfiles::new([StorageProfile::LocalFs]))
            .with_connection(connection, StorageProfiles::new([StorageProfile::Memory]));
        let memory = Location::parse("memory://warehouse/t").unwrap();
        assert_eq!(
            profiles
                .for_connection(connection)
                .for_location(&memory)
                .unwrap(),
            &StorageProfile::Memory
        );
        assert!(
            profiles
                .for_connection(CatalogConnectionId::generate())
                .for_location(&memory)
                .is_err()
        );
    }
}
