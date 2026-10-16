//! Runs an iceberg-rust transaction without letting it reach a catalog.
//!
//! `Transaction::commit` reloads the table, re-applies its actions on a
//! newer base, and retries conflicts, and the actions' own `commit` methods
//! are crate-private. So Bergsmith gives the transaction a catalog that
//! returns the baseline on every load and captures the commit instead of
//! sending it. The transaction sees an unchanged base, builds its updates
//! once, and stops at the captured commit with a non-retryable error.

use std::collections::HashMap;

use async_trait::async_trait;
use iceberg::table::Table;
use iceberg::transaction::Transaction;
use iceberg::{
    Catalog, Error, ErrorKind, Namespace, NamespaceIdent, TableCommit, TableCreation, TableIdent,
    TableRequirement, TableUpdate,
};
use tokio::sync::Mutex;

use crate::commit::error::ProposalError;

/// The error the capture catalog returns once it holds the commit.
const CAPTURED: &str = "bergsmith captured the commit instead of sending it";

/// A catalog that serves one table and captures its commit.
#[derive(Debug)]
struct CaptureCatalog {
    baseline: Table,
    captured: Mutex<Option<TableCommit>>,
}

impl CaptureCatalog {
    fn unsupported<T>() -> iceberg::Result<T> {
        Err(Error::new(
            ErrorKind::FeatureUnsupported,
            "the capture catalog serves only the baseline table",
        ))
    }
}

#[async_trait]
impl Catalog for CaptureCatalog {
    async fn list_namespaces(
        &self,
        _parent: Option<&NamespaceIdent>,
    ) -> iceberg::Result<Vec<NamespaceIdent>> {
        Self::unsupported()
    }

    async fn create_namespace(
        &self,
        _namespace: &NamespaceIdent,
        _properties: HashMap<String, String>,
    ) -> iceberg::Result<Namespace> {
        Self::unsupported()
    }

    async fn get_namespace(&self, _namespace: &NamespaceIdent) -> iceberg::Result<Namespace> {
        Self::unsupported()
    }

    async fn namespace_exists(&self, _namespace: &NamespaceIdent) -> iceberg::Result<bool> {
        Self::unsupported()
    }

    async fn update_namespace(
        &self,
        _namespace: &NamespaceIdent,
        _properties: HashMap<String, String>,
    ) -> iceberg::Result<()> {
        Self::unsupported()
    }

    async fn drop_namespace(&self, _namespace: &NamespaceIdent) -> iceberg::Result<()> {
        Self::unsupported()
    }

    async fn list_tables(&self, _namespace: &NamespaceIdent) -> iceberg::Result<Vec<TableIdent>> {
        Self::unsupported()
    }

    async fn create_table(
        &self,
        _namespace: &NamespaceIdent,
        _creation: TableCreation,
    ) -> iceberg::Result<Table> {
        Self::unsupported()
    }

    async fn load_table(&self, table: &TableIdent) -> iceberg::Result<Table> {
        if table == self.baseline.identifier() {
            Ok(self.baseline.clone())
        } else {
            Self::unsupported()
        }
    }

    async fn drop_table(&self, _table: &TableIdent) -> iceberg::Result<()> {
        Self::unsupported()
    }

    async fn purge_table(&self, _table: &TableIdent) -> iceberg::Result<()> {
        Self::unsupported()
    }

    async fn table_exists(&self, _table: &TableIdent) -> iceberg::Result<bool> {
        Self::unsupported()
    }

    async fn rename_table(&self, _src: &TableIdent, _dest: &TableIdent) -> iceberg::Result<()> {
        Self::unsupported()
    }

    async fn register_table(
        &self,
        _table: &TableIdent,
        _metadata_location: String,
    ) -> iceberg::Result<Table> {
        Self::unsupported()
    }

    async fn update_table(&self, commit: TableCommit) -> iceberg::Result<Table> {
        let mut captured = self.captured.lock().await;
        if captured.is_some() {
            return Err(Error::new(
                ErrorKind::Unexpected,
                "the transaction committed more than once",
            ));
        }
        *captured = Some(commit);
        Err(Error::new(ErrorKind::Unexpected, CAPTURED).with_retryable(false))
    }
}

/// Runs `transaction` against `baseline` and returns the commit it would
/// have sent.
pub(crate) async fn run(
    baseline: &Table,
    transaction: Transaction,
) -> Result<(Vec<TableUpdate>, Vec<TableRequirement>), ProposalError> {
    let catalog = CaptureCatalog {
        baseline: baseline.clone(),
        captured: Mutex::new(None),
    };
    let result = transaction.commit(&catalog).await;
    match (catalog.captured.into_inner(), result) {
        (Some(mut commit), Err(error)) if error.message() == CAPTURED => {
            Ok((commit.take_updates(), commit.take_requirements()))
        }
        (_, Err(error)) => Err(ProposalError::Iceberg(error)),
        (_, Ok(_)) => Err(ProposalError::NoChanges),
    }
}
