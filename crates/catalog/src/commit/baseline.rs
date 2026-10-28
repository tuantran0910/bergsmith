//! The planning baseline a commit is built against, and the standard REST
//! requirements that pin it.
//!
//! Requirements are derived once, from the baseline metadata, and never from
//! a reloaded table (BS-006.T2).

use std::sync::Arc;

use iceberg::io::FileIO;
use iceberg::spec::{MAIN_BRANCH, SnapshotRef, TableMetadata};
use iceberg::table::Table;
use iceberg::transaction::Transaction;
use iceberg::{NamespaceIdent, Runtime, TableIdent, TableRequirement};

use crate::commit::capture;
use crate::commit::error::ProposalError;
use crate::commit::request::CommitRequest;
use crate::connection::{LoadedTable, TableName};

/// A table exactly as it was when a plan captured it.
///
/// Commits built from a baseline assert its table UUID, `main` snapshot,
/// current schema ID, last assigned field ID, default partition-spec ID, last
/// assigned partition field ID, and default sort-order ID. The catalog rejects
/// the commit if any of them changed.
pub struct Baseline {
    name: TableName,
    table: Table,
    requirements: Vec<TableRequirement>,
}

impl Baseline {
    /// Pins `loaded` as a baseline. `file_io` must be able to write under
    /// the table's metadata location, where commits write manifests.
    ///
    /// # Errors
    ///
    /// [`ProposalError::Iceberg`] when the table cannot be built, such as for
    /// an invalid namespace or outside a Tokio runtime.
    pub fn new(loaded: &LoadedTable, file_io: FileIO) -> Result<Self, ProposalError> {
        let namespace = NamespaceIdent::from_vec(loaded.name.namespace.clone())
            .map_err(ProposalError::Iceberg)?;
        let mut builder = Table::builder()
            .identifier(TableIdent::new(namespace, loaded.name.name.clone()))
            .metadata(Arc::new(loaded.metadata.clone()))
            .file_io(file_io)
            .runtime(Runtime::try_current().map_err(ProposalError::Iceberg)?);
        if let Some(location) = &loaded.metadata_location {
            builder = builder.metadata_location(location.clone());
        }
        Ok(Self {
            name: loaded.name.clone(),
            table: builder.build().map_err(ProposalError::Iceberg)?,
            requirements: requirements_of(&loaded.metadata),
        })
    }

    /// The table name commits are sent to.
    #[must_use]
    pub fn name(&self) -> &TableName {
        &self.name
    }

    /// The iceberg-rust table.
    pub(crate) fn table(&self) -> &Table {
        &self.table
    }

    /// The baseline metadata file, if the table was not staged.
    #[must_use]
    pub fn metadata_location(&self) -> Option<&str> {
        self.table.metadata_location()
    }

    /// The iceberg-rust identifier of the table.
    pub(crate) fn identifier(&self) -> &TableIdent {
        self.table.identifier()
    }

    /// The baseline metadata.
    #[must_use]
    pub fn metadata(&self) -> &TableMetadata {
        self.table.metadata()
    }

    /// The baseline `main` snapshot, if the table has one.
    #[must_use]
    pub fn main_snapshot(&self) -> Option<&SnapshotRef> {
        self.metadata().snapshot_for_ref(MAIN_BRANCH)
    }

    /// The requirements every commit from this baseline asserts.
    #[must_use]
    pub fn requirements(&self) -> &[TableRequirement] {
        &self.requirements
    }

    /// Builds a commit from iceberg-rust actions applied to the baseline.
    ///
    /// The actions see only the baseline: iceberg-rust's transaction runs
    /// against a catalog that returns the baseline on reload and captures the
    /// commit instead of sending it, so nothing is refreshed, re-applied, or
    /// retried. The commit asserts [`requirements`](Self::requirements) plus
    /// any further requirements the actions add.
    ///
    /// Use [`RewriteProposal`](crate::RewriteProposal) for rewrites; this
    /// is the building block for other maintenance actions and test fixtures.
    ///
    /// # Errors
    ///
    /// [`ProposalError::Iceberg`] when an action fails, and
    /// [`ProposalError::RequirementConflict`] when an action asserts a value
    /// that disagrees with the baseline.
    pub async fn prepare_with(
        &self,
        apply: impl FnOnce(Transaction) -> iceberg::Result<Transaction>,
    ) -> Result<CommitRequest, ProposalError> {
        let (updates, requirements) = self.capture(apply).await?;
        CommitRequest::encode(&self.name, self.identifier(), requirements, updates)
    }

    /// Builds a commit from updates derived from this baseline, without
    /// running a transaction.
    ///
    /// The request asserts exactly [`requirements`](Self::requirements), so
    /// the catalog applies `updates` only if the table is still this
    /// baseline. Callers MUST derive every update from this exact baseline
    /// metadata; an update computed from a reloaded or newer table can
    /// disagree with the asserted requirements and be rejected, or worse,
    /// commit a change the baseline never described. Use
    /// [`prepare_with`](Self::prepare_with) when iceberg-rust's actions can
    /// derive the updates.
    ///
    /// # Errors
    ///
    /// [`ProposalError::Encode`] when the request does not serialize.
    pub fn commit_with_updates(
        &self,
        updates: Vec<iceberg::TableUpdate>,
    ) -> Result<CommitRequest, ProposalError> {
        CommitRequest::encode(
            &self.name,
            self.identifier(),
            self.requirements.clone(),
            updates,
        )
    }

    /// Runs `apply` against the baseline and returns the updates and the
    /// merged requirements.
    pub(crate) async fn capture(
        &self,
        apply: impl FnOnce(Transaction) -> iceberg::Result<Transaction>,
    ) -> Result<(Vec<iceberg::TableUpdate>, Vec<TableRequirement>), ProposalError> {
        let transaction = apply(Transaction::new(&self.table)).map_err(ProposalError::Iceberg)?;
        let (updates, action_requirements) = capture::run(&self.table, transaction).await?;
        let requirements = merge(&self.requirements, action_requirements)?;
        Ok((updates, requirements))
    }
}

/// The baseline's full requirement set, in a fixed order.
pub(crate) fn requirements_of(metadata: &TableMetadata) -> Vec<TableRequirement> {
    vec![
        TableRequirement::UuidMatch {
            uuid: metadata.uuid(),
        },
        TableRequirement::RefSnapshotIdMatch {
            r#ref: MAIN_BRANCH.to_owned(),
            snapshot_id: metadata
                .snapshot_for_ref(MAIN_BRANCH)
                .map(|snapshot| snapshot.snapshot_id()),
        },
        TableRequirement::CurrentSchemaIdMatch {
            current_schema_id: metadata.current_schema_id(),
        },
        TableRequirement::LastAssignedFieldIdMatch {
            last_assigned_field_id: metadata.last_column_id(),
        },
        TableRequirement::DefaultSpecIdMatch {
            default_spec_id: metadata.default_partition_spec_id(),
        },
        TableRequirement::LastAssignedPartitionIdMatch {
            last_assigned_partition_id: metadata.last_partition_id(),
        },
        TableRequirement::DefaultSortOrderIdMatch {
            default_sort_order_id: metadata.default_sort_order_id(),
        },
    ]
}

/// The baseline requirements followed by the action's requirements that
/// assert something else. An action requirement that asserts the same thing
/// with a different value means the action did not run on the baseline.
fn merge(
    baseline: &[TableRequirement],
    action: Vec<TableRequirement>,
) -> Result<Vec<TableRequirement>, ProposalError> {
    let mut merged = baseline.to_vec();
    for requirement in action {
        match merged.iter().find(|r| same_assertion(r, &requirement)) {
            Some(existing) if *existing == requirement => {}
            Some(existing) => {
                return Err(ProposalError::RequirementConflict {
                    baseline: Box::new(existing.clone()),
                    action: Box::new(requirement),
                });
            }
            None => merged.push(requirement),
        }
    }
    Ok(merged)
}

/// Whether two requirements assert the same property, whatever its value.
fn same_assertion(a: &TableRequirement, b: &TableRequirement) -> bool {
    match (a, b) {
        (
            TableRequirement::RefSnapshotIdMatch { r#ref: a, .. },
            TableRequirement::RefSnapshotIdMatch { r#ref: b, .. },
        ) => a == b,
        _ => std::mem::discriminant(a) == std::mem::discriminant(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_requirement(r#ref: &str, snapshot_id: Option<i64>) -> TableRequirement {
        TableRequirement::RefSnapshotIdMatch {
            r#ref: r#ref.to_owned(),
            snapshot_id,
        }
    }

    #[test]
    fn matching_action_requirements_are_not_repeated() {
        let baseline = vec![snapshot_requirement("main", Some(1))];

        let merged = merge(&baseline, vec![snapshot_requirement("main", Some(1))]).unwrap();

        assert_eq!(merged, baseline);
    }

    #[test]
    fn action_requirements_on_other_properties_are_added() {
        let baseline = vec![snapshot_requirement("main", Some(1))];
        let other = snapshot_requirement("audit", None);
        let sort = TableRequirement::DefaultSortOrderIdMatch {
            default_sort_order_id: 0,
        };

        let merged = merge(&baseline, vec![other.clone(), sort.clone()]).unwrap();

        assert_eq!(merged, [baseline[0].clone(), other, sort]);
    }

    #[test]
    fn action_requirements_that_disagree_with_the_baseline_are_rejected() {
        let baseline = vec![snapshot_requirement("main", Some(1))];

        let error = merge(&baseline, vec![snapshot_requirement("main", Some(2))]).unwrap_err();

        assert!(
            matches!(error, ProposalError::RequirementConflict { .. }),
            "{error}"
        );
    }
}
