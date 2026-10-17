//! Table checks and file eligibility (BS-009.T2).

use std::fmt;

use iceberg::spec::{DataFileFormat, FormatVersion, PrimitiveType, Schema, TableMetadata, Type};
use serde::{Deserialize, Serialize};

use crate::config::PlannerConfig;
use crate::deletes::DeleteIndex;
use crate::inventory::{DeleteKind, Inventory};
use crate::selection::Selection;

/// The most file-level rejections a preview lists. The first ones by path
/// are enough to explain the refusal.
const MAX_FILE_REJECTIONS: usize = 20;

/// Why the MVP refuses to plan a table. A preview lists every reason found.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum Rejection {
    /// Only format version 2 is supported.
    FormatVersion {
        /// The table's format version.
        version: i32,
    },
    /// The table has a default sort order, and the MVP never sorts.
    SortedTable {
        /// The default sort-order ID.
        sort_order_id: i64,
    },
    /// `encryption.key-id` is set.
    Encrypted,
    /// `write.format.default` is not Parquet.
    WriteFormat {
        /// The configured format.
        format: String,
    },
    /// A live data or delete file is not Parquet.
    NonParquetFile {
        /// The file.
        path: String,
        /// Its format.
        format: String,
    },
    /// An equality delete uses a key the delete path cannot compare.
    EqualityKey {
        /// The equality delete file.
        path: String,
        /// The key field.
        field_id: i32,
        /// What is wrong with it.
        problem: KeyProblem,
    },
}

/// What makes an equality key unsupported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyProblem {
    /// A float or double field: NaN makes equality ill-defined.
    FloatingPoint,
    /// A field nested in a struct, list, or map, or a non-primitive field.
    Nested,
    /// The field is not in the current schema.
    NotInSchema,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FormatVersion { version } => {
                write!(f, "format version {version} is not supported; only v2 is")
            }
            Self::SortedTable { sort_order_id } => write!(
                f,
                "the table has default sort order {sort_order_id}, and sorting is not supported"
            ),
            Self::Encrypted => f.write_str("encrypted tables are not supported"),
            Self::WriteFormat { format } => {
                write!(f, "write format {format} is not supported; only Parquet is")
            }
            Self::NonParquetFile { path, format } => {
                write!(f, "{path} is {format}; only Parquet files are supported")
            }
            Self::EqualityKey {
                path,
                field_id,
                problem,
            } => {
                let problem = match problem {
                    KeyProblem::FloatingPoint => "is floating-point",
                    KeyProblem::Nested => "is not a top-level primitive field",
                    KeyProblem::NotInSchema => "is not in the current schema",
                };
                write!(
                    f,
                    "equality delete {path} is keyed on field {field_id}, which {problem}"
                )
            }
        }
    }
}

/// Why a live data file is not a compaction candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The selection does not include its partition.
    OutsideSelection,
    /// It is larger than the group input limit.
    TooLarge,
    /// It is at least the minimum file size and has no applicable deletes.
    AtTargetSize,
}

/// Rejections decided from table metadata alone.
pub(crate) fn table_rejections(metadata: &TableMetadata) -> Vec<Rejection> {
    let mut rejections = Vec::new();
    if metadata.format_version() != FormatVersion::V2 {
        rejections.push(Rejection::FormatVersion {
            version: i32::from(metadata.format_version() as u8),
        });
    }
    if !metadata.default_sort_order().is_unsorted() {
        rejections.push(Rejection::SortedTable {
            sort_order_id: metadata.default_sort_order_id(),
        });
    }
    let properties = metadata.properties();
    if properties.contains_key("encryption.key-id") {
        rejections.push(Rejection::Encrypted);
    }
    if let Some(format) = properties.get("write.format.default")
        && !format.eq_ignore_ascii_case("parquet")
    {
        rejections.push(Rejection::WriteFormat {
            format: format.clone(),
        });
    }
    rejections
}

/// Rejections decided from the live files.
pub(crate) fn file_rejections(inventory: &Inventory, schema: &Schema) -> Vec<Rejection> {
    let mut rejections = Vec::new();
    let files = inventory
        .data
        .iter()
        .chain(inventory.deletes.iter().map(|delete| &delete.file));
    for file in files {
        if file.format != DataFileFormat::Parquet {
            rejections.push(Rejection::NonParquetFile {
                path: file.path.clone(),
                format: file.format.to_string(),
            });
        }
    }
    for delete in &inventory.deletes {
        let DeleteKind::Equality { field_ids } = &delete.kind else {
            continue;
        };
        for &field_id in field_ids {
            if let Some(problem) = key_problem(schema, field_id) {
                rejections.push(Rejection::EqualityKey {
                    path: delete.file.path.clone(),
                    field_id,
                    problem,
                });
            }
        }
    }
    rejections.sort();
    rejections.truncate(MAX_FILE_REJECTIONS);
    rejections
}

fn key_problem(schema: &Schema, field_id: i32) -> Option<KeyProblem> {
    let Some(field) = schema.as_struct().field_by_id(field_id) else {
        return Some(if schema.field_by_id(field_id).is_some() {
            KeyProblem::Nested
        } else {
            KeyProblem::NotInSchema
        });
    };
    match field.field_type.as_ref() {
        Type::Primitive(PrimitiveType::Float | PrimitiveType::Double) => {
            Some(KeyProblem::FloatingPoint)
        }
        Type::Primitive(_) => None,
        _ => Some(KeyProblem::Nested),
    }
}

/// A data file selected for compaction, by index into the inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) data: usize,
    /// Indices of the applicable delete files, sorted by path.
    pub(crate) deletes: Vec<usize>,
}

/// Each data file's decision, in inventory order.
pub(crate) fn classify(
    inventory: &Inventory,
    index: &DeleteIndex<'_>,
    selection: &Selection,
    config: &PlannerConfig,
) -> Vec<Result<Candidate, SkipReason>> {
    let position_of = |path: &str| {
        inventory
            .deletes
            .binary_search_by(|delete| delete.file.path.as_str().cmp(path))
            .ok()
    };
    inventory
        .data
        .iter()
        .enumerate()
        .map(|(data, file)| {
            if !selection.matches(&file.partition) {
                return Err(SkipReason::OutsideSelection);
            }
            if file.size > config.max_group_input_bytes {
                return Err(SkipReason::TooLarge);
            }
            let deletes: Vec<usize> = index
                .applicable(file)
                .into_iter()
                .filter_map(|delete| position_of(&delete.file.path))
                .collect();
            if file.size >= config.min_file_size_bytes && deletes.is_empty() {
                return Err(SkipReason::AtTargetSize);
            }
            Ok(Candidate { data, deletes })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use iceberg::spec::{ListType, NestedField, StructType};

    use super::*;
    use crate::deletes::tests::file;
    use crate::inventory::DeleteEntry;
    use crate::partition::PartitionKey;

    fn schema() -> Schema {
        let inner = NestedField::optional(3, "inner", Type::Primitive(PrimitiveType::Int));
        let element = NestedField::list_element(5, Type::Primitive(PrimitiveType::Int), false);
        Schema::builder()
            .with_fields([
                Arc::new(NestedField::required(
                    1,
                    "id",
                    Type::Primitive(PrimitiveType::Long),
                )),
                Arc::new(NestedField::optional(
                    2,
                    "outer",
                    Type::Struct(StructType::new(vec![Arc::new(inner)])),
                )),
                Arc::new(NestedField::optional(
                    4,
                    "items",
                    Type::List(ListType::new(Arc::new(element))),
                )),
                Arc::new(NestedField::optional(
                    6,
                    "score",
                    Type::Primitive(PrimitiveType::Float),
                )),
            ])
            .build()
            .unwrap()
    }

    fn equality(path: &str, field_ids: Vec<i32>) -> DeleteEntry {
        DeleteEntry {
            file: file(path, 2, PartitionKey::unpartitioned()),
            kind: DeleteKind::Equality { field_ids },
        }
    }

    #[test]
    fn equality_keys_must_be_top_level_non_floating_primitives() {
        let schema = schema();

        assert_eq!(key_problem(&schema, 1), None);
        assert_eq!(key_problem(&schema, 2), Some(KeyProblem::Nested));
        assert_eq!(key_problem(&schema, 3), Some(KeyProblem::Nested));
        assert_eq!(key_problem(&schema, 4), Some(KeyProblem::Nested));
        assert_eq!(key_problem(&schema, 6), Some(KeyProblem::FloatingPoint));
        assert_eq!(key_problem(&schema, 9), Some(KeyProblem::NotInSchema));
    }

    #[test]
    fn non_parquet_files_are_rejected_in_path_order_and_capped() {
        let mut inventory = Inventory::default();
        for index in 0..30 {
            let mut data = file(&format!("d{index:02}"), 1, PartitionKey::unpartitioned());
            data.format = DataFileFormat::Orc;
            inventory.data.push(data);
        }
        let mut delete = equality("e", vec![1]);
        delete.file.format = DataFileFormat::Avro;
        inventory.deletes.push(delete);

        let rejections = file_rejections(&inventory, &schema());

        assert_eq!(rejections.len(), MAX_FILE_REJECTIONS);
        assert_eq!(
            rejections[0],
            Rejection::NonParquetFile {
                path: "d00".into(),
                format: "orc".into()
            }
        );
        assert!(rejections[0].to_string().contains("only Parquet"));
    }

    #[test]
    fn every_unsupported_key_is_listed() {
        let inventory = Inventory {
            data: Vec::new(),
            deletes: vec![equality("e", vec![1, 3, 6])],
        };

        let rejections = file_rejections(&inventory, &schema());

        assert_eq!(
            rejections,
            [
                Rejection::EqualityKey {
                    path: "e".into(),
                    field_id: 3,
                    problem: KeyProblem::Nested
                },
                Rejection::EqualityKey {
                    path: "e".into(),
                    field_id: 6,
                    problem: KeyProblem::FloatingPoint
                },
            ]
        );
        assert!(
            rejections[0]
                .to_string()
                .contains("not a top-level primitive")
        );
    }

    #[test]
    fn rejections_serialize_with_a_reason_tag() {
        let json = serde_json::to_value(Rejection::SortedTable { sort_order_id: 3 }).unwrap();

        assert_eq!(
            json,
            serde_json::json!({"reason": "sorted_table", "sort_order_id": 3})
        );
    }
}
