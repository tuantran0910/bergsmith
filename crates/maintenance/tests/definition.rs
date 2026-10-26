//! Table definitions from metadata (BS-026.T3).

// clippy's `allow-unwrap-in-tests` and `allow-panic-in-tests` cover only
// `#[test]` functions; the helpers fail the test on setup errors.
#![expect(clippy::unwrap_used, reason = "test helpers fail the test")]

use std::collections::HashMap;
use std::sync::Arc;

use bergsmith_fixtures::{Commit, FileSpec, Row, TableSpec, Value, row};
use bergsmith_maintenance::{DEFINITION_PROPERTIES, TableDefinition};
use iceberg::spec::{
    FormatVersion, ListType, MapType, NestedField, NullOrder, PrimitiveType, Schema, SortDirection,
    SortField, SortOrder, StructType, TableMetadata, TableMetadataBuilder, Transform, Type,
    UnboundPartitionSpec,
};
use uuid::Uuid;

const ID: i32 = 1;
const TAG: i32 = 2;

fn schema_with_struct() -> Schema {
    Schema::builder()
        .with_fields([
            NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long))
                .with_doc("row key")
                .into(),
            NestedField::optional(TAG, "tag", Type::Primitive(PrimitiveType::String)).into(),
            NestedField::optional(
                3,
                "point",
                Type::Struct(StructType::new(vec![
                    NestedField::optional(4, "x", Type::Primitive(PrimitiveType::Double)).into(),
                    NestedField::optional(5, "y", Type::Primitive(PrimitiveType::Double)).into(),
                ])),
            )
            .into(),
            NestedField::optional(
                6,
                "tags",
                Type::List(ListType::new(
                    NestedField::optional(7, "element", Type::Primitive(PrimitiveType::String))
                        .into(),
                )),
            )
            .into(),
            NestedField::optional(
                8,
                "attributes",
                Type::Map(MapType::new(
                    NestedField::required(9, "key", Type::Primitive(PrimitiveType::String)).into(),
                    NestedField::optional(10, "value", Type::Primitive(PrimitiveType::Long)).into(),
                )),
            )
            .into(),
        ])
        .build()
        .unwrap()
}

fn by_tag() -> UnboundPartitionSpec {
    UnboundPartitionSpec::builder()
        .add_partition_field(TAG, "tag", Transform::Identity)
        .unwrap()
        .build()
}

fn by_truncate() -> UnboundPartitionSpec {
    UnboundPartitionSpec::builder()
        .add_partition_field(TAG, "tag_prefix", Transform::Truncate(2))
        .unwrap()
        .build()
}

fn sorted() -> SortOrder {
    SortOrder {
        order_id: 1,
        fields: vec![SortField {
            source_id: ID,
            transform: Transform::Identity,
            direction: SortDirection::Descending,
            null_order: NullOrder::First,
        }],
    }
}

fn metadata() -> TableMetadata {
    // A property outside the allow-list must never reach the definition.
    let properties = HashMap::from([
        ("write.format.default".to_owned(), "parquet".to_owned()),
        (
            "write.target-file-size-bytes".to_owned(),
            "1048576".to_owned(),
        ),
        ("custom.private".to_owned(), "not reported".to_owned()),
    ]);
    TableMetadataBuilder::new(
        schema_with_struct(),
        by_tag(),
        sorted(),
        "memory://warehouse/t".to_owned(),
        FormatVersion::V2,
        properties,
    )
    .unwrap()
    .add_default_partition_spec(by_truncate())
    .unwrap()
    .assign_uuid(Uuid::from_u128(7))
    .build()
    .unwrap()
    .metadata
}

#[test]
fn definition_reports_the_current_schema_and_every_spec() {
    let definition = TableDefinition::from_metadata(&metadata());

    assert_eq!(definition.format_version, "v2");
    assert_eq!(definition.schema.schema_id, 0);
    let fields = &definition.schema.fields;
    assert_eq!(fields.len(), 5);
    assert_eq!(fields[0].id, ID);
    assert_eq!(fields[0].name, "id");
    assert_eq!(fields[0].field_type, "long");
    assert!(fields[0].required);
    assert_eq!(fields[0].doc.as_deref(), Some("row key"));
    assert_eq!(fields[1].field_type, "string");
    assert!(!fields[1].required);
    assert_eq!(
        fields[2].field_type,
        "struct<6: x: optional double, 7: y: optional double>"
    );
    assert_eq!(fields[3].field_type, "list<8: element: optional string>");
    assert_eq!(
        fields[4].field_type,
        "map<9: key: required string, 10: value: optional long>"
    );

    let specs = &definition.partition_specs;
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0].spec_id, 0);
    assert!(!specs[0].default);
    assert_eq!(specs[0].fields[0].name, "tag");
    assert_eq!(specs[0].fields[0].source_id, TAG);
    assert_eq!(specs[0].fields[0].transform, "identity");
    assert_eq!(specs[1].spec_id, 1);
    assert!(specs[1].default);
    assert_eq!(specs[1].fields[0].transform, "truncate[2]");
}

#[test]
fn definition_reports_the_default_sort_order() {
    let definition = TableDefinition::from_metadata(&metadata());

    assert_eq!(definition.sort_order.order_id, 1);
    assert_eq!(definition.sort_order.fields.len(), 1);
    let field = &definition.sort_order.fields[0];
    assert_eq!(field.source_id, ID);
    assert_eq!(field.transform, "identity");
    assert_eq!(field.direction, "desc");
    assert_eq!(field.null_order, "nulls-first");
}

#[test]
fn definition_reports_only_allow_listed_properties_that_are_set() {
    let definition = TableDefinition::from_metadata(&metadata());

    assert_eq!(
        definition.properties.keys().collect::<Vec<_>>(),
        vec!["write.format.default", "write.target-file-size-bytes"]
    );
    assert!(DEFINITION_PROPERTIES.contains(&"write.format.default"));
}

#[test]
fn an_unsorted_unpartitioned_table_has_empty_views() {
    let unsorted = TableMetadataBuilder::new(
        Schema::builder()
            .with_fields([Arc::new(NestedField::required(
                ID,
                "id",
                Type::Primitive(PrimitiveType::Long),
            ))])
            .build()
            .unwrap(),
        UnboundPartitionSpec::builder().build(),
        SortOrder::unsorted_order(),
        "memory://warehouse/t".to_owned(),
        FormatVersion::V2,
        HashMap::new(),
    )
    .unwrap()
    .assign_uuid(Uuid::from_u128(8))
    .build()
    .unwrap()
    .metadata;

    let definition = TableDefinition::from_metadata(&unsorted);
    assert_eq!(definition.partition_specs.len(), 1);
    assert!(definition.partition_specs[0].default);
    assert!(definition.partition_specs[0].fields.is_empty());
    assert_eq!(definition.sort_order.order_id, 0);
    assert!(definition.sort_order.fields.is_empty());
    assert!(definition.properties.is_empty());
}

/// `count` rows tagged `tag`.
fn rows(count: i64, tag: &str) -> Vec<Row> {
    (0..count).map(|id| row![ID => id, TAG => tag]).collect()
}

fn data(name: &str, count: i64) -> FileSpec {
    FileSpec::data(name, rows(count, name))
}

fn tagged_schema() -> Schema {
    Schema::builder()
        .with_fields([
            Arc::new(NestedField::required(
                ID,
                "id",
                Type::Primitive(PrimitiveType::Long),
            )),
            Arc::new(NestedField::optional(
                TAG,
                "tag",
                Type::Primitive(PrimitiveType::String),
            )),
        ])
        .build()
        .unwrap()
}

/// The native fixture keeps an old partition spec beside the evolved one, so
/// a definition of real table metadata still reports both.
#[tokio::test]
async fn definition_of_a_native_fixture_reports_old_specs() {
    let fixture = TableSpec::new(tagged_schema())
        .partitioned_by(by_tag())
        .commit(
            Commit::new()
                .file(data("x0", 2).partition([Value::string("x")]))
                .file(data("x1", 2).partition([Value::string("x")])),
        )
        .commit(Commit::new().evolve_spec(UnboundPartitionSpec::builder().build()))
        .generate()
        .await
        .unwrap();

    let definition = TableDefinition::from_metadata(fixture.metadata());
    assert_eq!(definition.partition_specs.len(), 2);
    assert_eq!(definition.partition_specs[0].spec_id, 0);
    assert_eq!(definition.partition_specs[0].fields[0].name, "tag");
    assert_eq!(definition.partition_specs[1].spec_id, 1);
    assert!(definition.partition_specs[1].default);
    assert!(definition.partition_specs[1].fields.is_empty());
    assert_eq!(definition.schema.schema_id, 0);
    assert_eq!(definition.schema.fields[0].name, "id");
}
