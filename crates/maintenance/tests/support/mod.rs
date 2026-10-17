//! Tables shared by the planner tests.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of the helpers"
)]

use std::collections::HashMap;
use std::sync::Arc;

use bergsmith_fixtures::{FileSpec, Row, row};
use iceberg::spec::{
    NestedField, PrimitiveType, Schema, TableMetadata, TableMetadataBuilder, Transform, Type,
    UnboundPartitionSpec,
};

pub(crate) const ID: i32 = 1;
pub(crate) const TAG: i32 = 2;

pub(crate) fn schema(fields: impl IntoIterator<Item = NestedField>) -> Schema {
    Schema::builder()
        .with_fields(fields.into_iter().map(Arc::new))
        .build()
        .unwrap()
}

/// `id long required, tag string`.
pub(crate) fn tagged() -> Schema {
    schema([
        NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::optional(TAG, "tag", Type::Primitive(PrimitiveType::String)),
    ])
}

/// An identity spec on `tag`.
pub(crate) fn by_tag() -> UnboundPartitionSpec {
    UnboundPartitionSpec::builder()
        .add_partition_field(TAG, "tag", Transform::Identity)
        .unwrap()
        .build()
}

/// `count` rows tagged `tag`, with ids from 0.
pub(crate) fn rows(count: i64, tag: &str) -> Vec<Row> {
    (0..count).map(|id| row![ID => id, TAG => tag]).collect()
}

/// A data file of `count` rows tagged with its name.
pub(crate) fn data(name: &str, count: i64) -> FileSpec {
    FileSpec::data(name, rows(count, name))
}

/// `metadata` with `properties` set, as a later metadata-only commit would
/// leave it.
pub(crate) fn with_properties(
    metadata: &TableMetadata,
    properties: &[(&str, &str)],
) -> TableMetadata {
    let properties: HashMap<String, String> = properties
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    TableMetadataBuilder::new_from_metadata(metadata.clone(), None)
        .set_properties(properties)
        .unwrap()
        .build()
        .unwrap()
        .metadata
}

/// Previews `fixture` with `config` and `selection`.
pub(crate) async fn preview_with(
    fixture: &bergsmith_fixtures::Fixture,
    metadata: &TableMetadata,
    config: bergsmith_maintenance::PlannerConfig,
    selection: &[bergsmith_maintenance::PartitionPredicate],
) -> Result<bergsmith_maintenance::Preview, bergsmith_maintenance::PlanError> {
    bergsmith_maintenance::Planner::new(config)
        .unwrap()
        .preview(bergsmith_maintenance::PlanInput {
            metadata,
            metadata_location: Some(fixture.metadata_location()),
            file_io: fixture.file_io(),
            selection,
        })
        .await
}

/// A configuration in which every file is at least the minimum size, so
/// only files with deletes are candidates.
pub(crate) fn deletes_only() -> bergsmith_maintenance::PlannerConfig {
    bergsmith_maintenance::PlannerConfig {
        min_file_size_bytes: 1,
        ..bergsmith_maintenance::PlannerConfig::default()
    }
}
