//! The current definition of a table: its schema, partition specs, sort
//! order, and the properties an operator needs (BS-026.T3).
//!
//! Everything here is derived from table metadata the coordinator already
//! loads; no data or delete file is opened. Partition *values* are not part of
//! a definition, so this document carries no per-partition counts.

use std::collections::BTreeMap;

use iceberg::spec::{NestedFieldRef, NullOrder, SortDirection, TableMetadata, Type};
use serde::{Deserialize, Serialize};

/// Table properties a definition reports.
///
/// This is a documented allow-list, not "every property the table sets": a
/// catalog or engine may store keys Bergsmith does not understand or should
/// not surface, and an allow-list keeps the payload stable when it does. The
/// list covers the write layout, the table format, and the metadata retention
/// an operator needs to explain a maintenance result. Unset properties are
/// omitted rather than reported as defaults, because the defaults belong to
/// the engine, not to this table.
pub const DEFINITION_PROPERTIES: [&str; 14] = [
    "encryption.key-id",
    "write.avro.compression-codec",
    "write.data.path",
    "write.distribution-mode",
    "write.format.default",
    "write.metadata.compression-codec",
    "write.metadata.delete-after-commit.enabled",
    "write.metadata.path",
    "write.metadata.previous-versions-max",
    "write.object-storage.enabled",
    "write.object-storage.path",
    "write.orc.compression-codec",
    "write.parquet.compression-codec",
    "write.target-file-size-bytes",
];

/// One table's current definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableDefinition {
    /// The table's Iceberg format version, such as `v2`.
    pub format_version: String,
    /// The current schema.
    pub schema: SchemaDefinition,
    /// Every partition spec the metadata retains, oldest spec ID first.
    pub partition_specs: Vec<SpecDefinition>,
    /// The table's default sort order.
    pub sort_order: SortOrderDefinition,
    /// The [`DEFINITION_PROPERTIES`] that are set, in key order.
    pub properties: BTreeMap<String, String>,
}

/// The current schema, as top-level fields in schema order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaDefinition {
    /// The schema ID.
    pub schema_id: i32,
    /// Top-level fields. A nested type appears in the field's type text.
    pub fields: Vec<FieldDefinition>,
}

/// One top-level schema field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDefinition {
    /// The field ID.
    pub id: i32,
    /// The field name.
    pub name: String,
    /// The field type as Iceberg renders it, with nested field IDs and names,
    /// such as `struct<4: x: optional double>`.
    #[serde(rename = "type")]
    pub field_type: String,
    /// Whether every row must supply the field.
    pub required: bool,
    /// The field's documentation, when the schema records it.
    pub doc: Option<String>,
}

/// One partition spec, including specs older files still reference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecDefinition {
    /// The spec ID.
    pub spec_id: i32,
    /// Whether this is the table's default spec.
    pub default: bool,
    /// The spec's fields, in spec order. Empty for an unpartitioned spec.
    pub fields: Vec<SpecFieldDefinition>,
}

/// One partition field of a spec.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecFieldDefinition {
    /// The schema field the transform reads.
    pub source_id: i32,
    /// The partition field ID.
    pub field_id: i32,
    /// The partition field name.
    pub name: String,
    /// The transform as Iceberg renders it, such as `identity` or `day`.
    pub transform: String,
}

/// The table's default sort order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SortOrderDefinition {
    /// The order ID. Zero is the unsorted order.
    pub order_id: i64,
    /// Sort fields in application order. Empty when the table is unsorted.
    pub fields: Vec<SortFieldDefinition>,
}

/// One sort field of the default sort order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SortFieldDefinition {
    /// The schema field the transform reads.
    pub source_id: i32,
    /// The transform as Iceberg renders it.
    pub transform: String,
    /// `asc` or `desc`.
    pub direction: String,
    /// `nulls-first` or `nulls-last`.
    pub null_order: String,
}

impl TableDefinition {
    /// Reads `metadata` into the definition the explorer shows.
    ///
    /// The result is ordered: specs by ID, schema fields in schema order, sort
    /// fields in order, and properties by key, so two definitions of one
    /// metadata file serialize identically.
    #[must_use]
    pub fn from_metadata(metadata: &TableMetadata) -> Self {
        let schema = metadata.current_schema();
        let default_spec_id = metadata.default_partition_spec_id();
        let mut partition_specs: Vec<SpecDefinition> = metadata
            .partition_specs_iter()
            .map(|spec| SpecDefinition {
                spec_id: spec.spec_id(),
                default: spec.spec_id() == default_spec_id,
                fields: spec
                    .fields()
                    .iter()
                    .map(|field| SpecFieldDefinition {
                        source_id: field.source_id,
                        field_id: field.field_id,
                        name: field.name.clone(),
                        transform: field.transform.to_string(),
                    })
                    .collect(),
            })
            .collect();
        partition_specs.sort_by_key(|spec| spec.spec_id);
        let sort_order = metadata.default_sort_order();
        let properties = DEFINITION_PROPERTIES
            .iter()
            .filter_map(|key| {
                let value = metadata.properties().get(*key)?;
                Some(((*key).to_owned(), value.clone()))
            })
            .collect();
        Self {
            format_version: metadata.format_version().to_string(),
            schema: SchemaDefinition {
                schema_id: schema.schema_id(),
                fields: schema
                    .as_struct()
                    .fields()
                    .iter()
                    .map(|field| FieldDefinition {
                        id: field.id,
                        name: field.name.clone(),
                        field_type: type_text(&field.field_type),
                        required: field.required,
                        doc: field.doc.clone(),
                    })
                    .collect(),
            },
            partition_specs,
            sort_order: SortOrderDefinition {
                order_id: sort_order.order_id,
                fields: sort_order
                    .fields
                    .iter()
                    .map(|field| SortFieldDefinition {
                        source_id: field.source_id,
                        transform: field.transform.to_string(),
                        direction: match field.direction {
                            SortDirection::Ascending => "asc",
                            SortDirection::Descending => "desc",
                        }
                        .to_owned(),
                        null_order: match field.null_order {
                            NullOrder::First => "nulls-first",
                            NullOrder::Last => "nulls-last",
                        }
                        .to_owned(),
                    })
                    .collect(),
            },
            properties,
        }
    }
}

/// Renders a type with nested field IDs and names, so a struct or map reads
/// as `struct<4: x: optional double>` instead of Iceberg's name-free
/// `struct<doubledouble>`.
fn type_text(field_type: &Type) -> String {
    match field_type {
        Type::Primitive(primitive) => primitive.to_string(),
        Type::Struct(strukt) => format!(
            "struct<{}>",
            strukt
                .fields()
                .iter()
                .map(|field| format!("{}: {}", field.id, field_text(field)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::List(list) => format!(
            "list<{}: {}>",
            list.element_field.id,
            field_text(&list.element_field)
        ),
        Type::Map(map) => format!(
            "map<{}, {}>",
            field_text(&map.key_field),
            field_text(&map.value_field)
        ),
        Type::Variant(_) => "variant".to_owned(),
    }
}

/// One nested field as `name: optional|required type`.
fn field_text(field: &NestedFieldRef) -> String {
    format!(
        "{}: {} {}",
        field.name,
        if field.required {
            "required"
        } else {
            "optional"
        },
        type_text(&field.field_type)
    )
}
