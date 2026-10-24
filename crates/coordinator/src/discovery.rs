//! Bounded catalog discovery and table registration (BS-022.T4).
//!
//! Browsing and registering are outbound catalog calls an administrator
//! triggers by hand. They are bounded three times: a listing reads one page of
//! at most [`bergsmith_catalog::MAX_LISTING_ITEMS`] items and
//! [`bergsmith_catalog::MAX_LISTING_BYTES`] identifier bytes, one registration
//! records at most [`MAX_REGISTRATION_TABLES`] tables, and the process runs at
//! most [`MAX_CONCURRENT_DISCOVERY`] discovery operations at once, so a slow
//! or oversized catalog cannot starve planning, previews, or publication.
//!
//! Registration reads every table's metadata before it writes anything: the
//! ledger records either the whole request or none of it, and the caller's
//! transaction carries both the aliases and their audit records.

use std::sync::Arc;

use bergsmith_catalog::{Listing, TableName};
use bergsmith_persistence::tenancy::CatalogConnection as StoredConnection;
use futures::StreamExt as _;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::runtime::catalogs::{AccessError, RuntimeCatalogs};

/// The most discovery operations one process runs at once.
///
/// Every operation is itself bounded, so this bound caps how many catalogs one
/// administrator can address at a time, whatever the size of the request.
pub const MAX_CONCURRENT_DISCOVERY: usize = 4;

/// The most tables one registration records.
///
/// A namespace with more tables is refused rather than half-registered: the
/// operator registers the tables that matter individually.
pub const MAX_REGISTRATION_TABLES: usize = 100;

/// How many tables one registration loads from the catalog at once.
pub const REGISTRATION_CONCURRENCY: usize = 8;

/// Bounds applied to catalog discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscoveryLimits {
    /// The most discovery operations one process runs at once.
    pub max_concurrent: usize,
    /// The most tables one registration records.
    pub max_registration_tables: usize,
    /// How many tables one registration loads at once.
    pub load_concurrency: usize,
}

impl Default for DiscoveryLimits {
    fn default() -> Self {
        Self {
            max_concurrent: MAX_CONCURRENT_DISCOVERY,
            max_registration_tables: MAX_REGISTRATION_TABLES,
            load_concurrency: REGISTRATION_CONCURRENCY,
        }
    }
}

/// Why discovery could not answer.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// Too many discovery operations are already running.
    #[error("catalog discovery capacity is exhausted")]
    Overloaded,
    /// The connection cannot be used as it is stored.
    #[error(transparent)]
    Access(#[from] AccessError),
    /// The catalog call failed.
    #[error(transparent)]
    Catalog(#[from] bergsmith_catalog::CatalogError),
    /// The catalog does not advertise the endpoint this operation needs.
    #[error("the catalog does not support {operation}")]
    Unsupported {
        /// The operation the catalog lacks, in the catalog's own terms.
        operation: &'static str,
    },
    /// One table could not be read from the catalog.
    #[error("catalog table {table} could not be read")]
    Table {
        /// The catalog name, as `namespace.table`.
        table: String,
        /// The catalog's failure.
        #[source]
        source: bergsmith_catalog::CatalogError,
    },
    /// The request named more tables than one registration takes.
    #[error("one registration records at most {limit} tables, but {requested} were named")]
    TooManyTables {
        /// How many tables the request named.
        requested: usize,
        /// The bound in force.
        limit: usize,
    },
    /// The catalog answered with a name outside the namespace it was asked
    /// about.
    ///
    /// Registering that name would record a table the request never named, so
    /// the answer is refused instead of trusted.
    #[error("the catalog answered {returned} for a listing of {requested}")]
    NamespaceMismatch {
        /// The namespace the request named, as `a.b`.
        requested: String,
        /// The namespace the catalog answered with.
        returned: String,
    },
}

/// Reads a connection's catalog within the process bounds.
#[derive(Clone)]
pub struct CatalogDiscovery {
    catalogs: Arc<RuntimeCatalogs>,
    limits: DiscoveryLimits,
    capacity: Arc<Semaphore>,
}

impl CatalogDiscovery {
    /// Creates discovery with explicit bounds.
    #[must_use]
    pub fn new(catalogs: Arc<RuntimeCatalogs>, limits: DiscoveryLimits) -> Self {
        let limits = DiscoveryLimits {
            max_concurrent: limits.max_concurrent.max(1),
            max_registration_tables: limits.max_registration_tables.max(1),
            load_concurrency: limits.load_concurrency.max(1),
        };
        Self {
            catalogs,
            limits,
            capacity: Arc::new(Semaphore::new(limits.max_concurrent)),
        }
    }

    /// The bounds in force.
    #[must_use]
    pub const fn limits(&self) -> DiscoveryLimits {
        self.limits
    }

    /// Lists one page of the namespaces under `parent`.
    ///
    /// `page_token` is a token from an earlier page, and `page_size` is what
    /// the catalog is asked for. The permit is taken without waiting: a queued
    /// browse would hold the request while it waits for capacity.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::Overloaded`] when the process is at its bound,
    /// [`DiscoveryError::Unsupported`] when the catalog does not list
    /// namespaces, and [`DiscoveryError::Access`] or
    /// [`DiscoveryError::Catalog`] when the connection or the call fails.
    pub async fn namespaces(
        &self,
        connection: &StoredConnection,
        parent: &[String],
        page_token: Option<&str>,
        page_size: usize,
    ) -> Result<Listing<Vec<String>>, DiscoveryError> {
        let _permit = self.permit()?;
        let catalog = self.catalogs.catalog_for(connection).await?;
        if !catalog.capabilities().supports_list_namespaces() {
            return Err(DiscoveryError::Unsupported {
                operation: "GET /v1/{prefix}/namespaces",
            });
        }
        let parent = if parent.is_empty() {
            None
        } else {
            Some(parent)
        };
        Ok(catalog
            .list_namespaces(parent, page_token, page_size)
            .await?)
    }

    /// Lists one page of the tables of a namespace.
    ///
    /// The namespace is nonempty; the route rejects an empty one before this
    /// call. A table the catalog places outside it is refused: a registration
    /// would otherwise record a name the request never asked for.
    ///
    /// # Errors
    ///
    /// [`Self::namespaces`]'s errors, without the namespace-listing
    /// requirement, and [`DiscoveryError::NamespaceMismatch`] when the catalog
    /// answered with a table in another namespace.
    pub async fn tables(
        &self,
        connection: &StoredConnection,
        namespace: &[String],
        page_token: Option<&str>,
        page_size: usize,
    ) -> Result<Listing<TableName>, DiscoveryError> {
        let _permit = self.permit()?;
        let catalog = self.catalogs.catalog_for(connection).await?;
        let listing = catalog
            .list_tables(namespace, page_token, page_size)
            .await?;
        if let Some(table) = listing
            .items
            .iter()
            .find(|table| table.namespace != namespace)
        {
            return Err(DiscoveryError::NamespaceMismatch {
                requested: namespace.join("."),
                returned: table.namespace.join("."),
            });
        }
        Ok(listing)
    }

    /// Loads every named table, so its identity can be recorded.
    ///
    /// Nothing is written: the caller records the aliases and their audit in
    /// one transaction, which keeps a partial registration impossible.
    ///
    /// # Errors
    ///
    /// [`DiscoveryError::TooManyTables`] for a request over the bound,
    /// [`DiscoveryError::Overloaded`] at the process bound, and
    /// [`DiscoveryError::Table`] naming the first table the catalog could not
    /// read in catalog order.
    pub async fn load(
        &self,
        connection: &StoredConnection,
        names: &[TableName],
    ) -> Result<Vec<(TableName, Uuid)>, DiscoveryError> {
        if names.len() > self.limits.max_registration_tables {
            return Err(DiscoveryError::TooManyTables {
                requested: names.len(),
                limit: self.limits.max_registration_tables,
            });
        }
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let _permit = self.permit()?;
        let catalog = self.catalogs.catalog_for(connection).await?;
        let mut loaded: Vec<(TableName, Result<Uuid, DiscoveryError>)> =
            futures::stream::iter(names.iter().cloned())
                .map(|name| {
                    let catalog = catalog.clone();
                    async move {
                        let loaded = catalog.load_table(&name, false).await.map_err(|source| {
                            DiscoveryError::Table {
                                table: display(&name),
                                source,
                            }
                        });
                        (name, loaded.map(|loaded| loaded.metadata.uuid()))
                    }
                })
                .buffer_unordered(self.limits.load_concurrency)
                .collect()
                .await;
        // The catalog's answer order is its own; a stable order keeps the
        // response and the audit record reproducible.
        loaded.sort_by(|(left, _), (right, _)| left.cmp(right));
        let mut identities = Vec::with_capacity(loaded.len());
        for (name, identity) in loaded {
            identities.push((name, identity?));
        }
        Ok(identities)
    }

    /// The ledger handle, so the caller can record the aliases it loaded.
    #[must_use]
    pub fn database(&self) -> &bergsmith_persistence::Database {
        self.catalogs.database()
    }

    /// Takes a discovery permit without waiting.
    fn permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, DiscoveryError> {
        self.capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| DiscoveryError::Overloaded)
    }
}

impl std::fmt::Debug for CatalogDiscovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogDiscovery")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// The catalog name of a table, as an operator reads it.
#[must_use]
pub fn display(name: &TableName) -> String {
    format!("{}.{}", name.namespace.join("."), name.name)
}
