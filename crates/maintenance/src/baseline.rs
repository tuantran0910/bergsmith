//! The planning baseline (BS-009.T1).
//!
//! A plan pins the table state it was built from. Publication asserts that
//! state through standard REST requirements and the drift check, so every
//! value here is captured once and never refreshed.

use std::collections::BTreeMap;

use bergsmith_common::table::PLAN_RELEVANT_PROPERTIES;
use bergsmith_storage::Location;
use iceberg::spec::{FormatVersion, MAIN_BRANCH, TableMetadata};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::PlanError;

/// The table property naming where data files are written.
const DATA_PATH_PROPERTY: &str = "write.data.path";

/// The table state a plan was built from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningBaseline {
    table_uuid: Uuid,
    metadata_location: String,
    format_version: i32,
    snapshot_id: i64,
    sequence_number: i64,
    schema_id: i32,
    last_column_id: i32,
    default_spec_id: i32,
    last_partition_id: i32,
    default_sort_order_id: i64,
    location: String,
    data_location: String,
    properties: BTreeMap<String, String>,
}

impl PlanningBaseline {
    /// Captures `metadata`, read from `metadata_location`, as a baseline.
    ///
    /// Returns `None` when the table has no `main` snapshot: there is
    /// nothing to compact, and the job ends as `NoOp`. The data location is
    /// `write.data.path`, or `<location>/data` when that is unset.
    ///
    /// # Errors
    ///
    /// - [`PlanError::StagedTable`] when `metadata_location` is `None`.
    /// - [`PlanError::Location`] when the table location or
    ///   `write.data.path` is not a valid storage location.
    pub fn capture(
        metadata: &TableMetadata,
        metadata_location: Option<&str>,
    ) -> Result<Option<Self>, PlanError> {
        let metadata_location = metadata_location.ok_or(PlanError::StagedTable {
            table_uuid: metadata.uuid(),
        })?;
        let location = Location::parse(metadata.location())?;
        let data_location = match metadata.properties().get(DATA_PATH_PROPERTY) {
            Some(path) => Location::parse(path)?,
            None => location.child("data")?,
        };
        let Some(snapshot) = metadata.snapshot_for_ref(MAIN_BRANCH) else {
            return Ok(None);
        };
        let properties = PLAN_RELEVANT_PROPERTIES
            .iter()
            .filter_map(|key| {
                let value = metadata.properties().get(*key)?;
                Some(((*key).to_owned(), value.clone()))
            })
            .collect();
        Ok(Some(Self {
            table_uuid: metadata.uuid(),
            metadata_location: metadata_location.to_owned(),
            format_version: format_version(metadata.format_version()),
            snapshot_id: snapshot.snapshot_id(),
            sequence_number: snapshot.sequence_number(),
            schema_id: metadata.current_schema_id(),
            last_column_id: metadata.last_column_id(),
            default_spec_id: metadata.default_partition_spec_id(),
            last_partition_id: metadata.last_partition_id(),
            default_sort_order_id: metadata.default_sort_order_id(),
            location: location.to_string(),
            data_location: data_location.to_string(),
            properties,
        }))
    }

    /// The Iceberg `table-uuid`.
    #[must_use]
    pub const fn table_uuid(&self) -> Uuid {
        self.table_uuid
    }

    /// The metadata file the plan was built from.
    #[must_use]
    pub fn metadata_location(&self) -> &str {
        &self.metadata_location
    }

    /// The Iceberg format version.
    #[must_use]
    pub const fn format_version(&self) -> i32 {
        self.format_version
    }

    /// The `main` snapshot.
    #[must_use]
    pub const fn snapshot_id(&self) -> i64 {
        self.snapshot_id
    }

    /// The `main` snapshot's sequence number, which replacement files take
    /// (ADR 0004).
    #[must_use]
    pub const fn sequence_number(&self) -> i64 {
        self.sequence_number
    }

    /// The current schema ID.
    #[must_use]
    pub const fn schema_id(&self) -> i32 {
        self.schema_id
    }

    /// The last assigned field ID.
    #[must_use]
    pub const fn last_column_id(&self) -> i32 {
        self.last_column_id
    }

    /// The default partition-spec ID.
    #[must_use]
    pub const fn default_spec_id(&self) -> i32 {
        self.default_spec_id
    }

    /// The last assigned partition field ID.
    #[must_use]
    pub const fn last_partition_id(&self) -> i32 {
        self.last_partition_id
    }

    /// The default sort-order ID.
    #[must_use]
    pub const fn default_sort_order_id(&self) -> i64 {
        self.default_sort_order_id
    }

    /// The table's base location.
    #[must_use]
    pub fn location(&self) -> &str {
        &self.location
    }

    /// Where data files are written: attempt outputs go under
    /// `<data-location>/bergsmith/` (ADR 0007).
    #[must_use]
    pub fn data_location(&self) -> &str {
        &self.data_location
    }

    /// The values of the [`PLAN_RELEVANT_PROPERTIES`] that are set.
    #[must_use]
    pub const fn properties(&self) -> &BTreeMap<String, String> {
        &self.properties
    }
}

const fn format_version(version: FormatVersion) -> i32 {
    match version {
        FormatVersion::V1 => 1,
        FormatVersion::V2 => 2,
        FormatVersion::V3 => 3,
    }
}
