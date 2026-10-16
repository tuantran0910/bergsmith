//! Canonical SHA-256 digests of task artifacts and receipts.
//!
//! A digest is `SHA-256(domain || 0x00 || protobuf encoding)` of the message
//! after canonicalization: every documented repeated field is sorted. prost
//! encodes fields in field-number order and the hashed messages have no maps
//! or floating-point fields, so the encoding of a canonical message is
//! deterministic. The domain prefix keeps an artifact digest from ever
//! matching a receipt digest.
//!
//! Stability: an unset proto3 field is not encoded, so adding a new field
//! leaves existing digests unchanged until the field is used. Changing a
//! field's number or type changes digests and is a breaking change.

pub use bergsmith_common::digest::{DigestLengthError, Sha256Digest};
use prost::Message;

use crate::artifact::{DeleteFile, OutputFile, PartitionKey, TaskArtifact, TaskReceipt};

const ARTIFACT_DOMAIN: &[u8] = b"bergsmith.artifact.v1.TaskArtifact";
const RECEIPT_DOMAIN: &[u8] = b"bergsmith.artifact.v1.TaskReceipt";

/// Returns the canonical digest of a task artifact, whatever the order of
/// its repeated fields.
#[must_use]
pub fn artifact_digest(artifact: &TaskArtifact) -> Sha256Digest {
    let mut canonical = artifact.clone();
    canonicalize_artifact(&mut canonical);
    digest(ARTIFACT_DOMAIN, &canonical)
}

/// Returns the canonical digest of a task receipt, whatever the order of its
/// repeated fields.
#[must_use]
pub fn receipt_digest(receipt: &TaskReceipt) -> Sha256Digest {
    let mut canonical = receipt.clone();
    canonicalize_receipt(&mut canonical);
    digest(RECEIPT_DOMAIN, &canonical)
}

/// Sorts every repeated field of an artifact into canonical order.
pub fn canonicalize_artifact(artifact: &mut TaskArtifact) {
    artifact.source_files.sort_by(|a, b| a.path.cmp(&b.path));
    artifact.delete_files.sort_by(|a, b| a.path.cmp(&b.path));
    for file in &mut artifact.source_files {
        canonicalize_partition(file.partition.as_mut());
    }
    for file in &mut artifact.delete_files {
        canonicalize_delete(file);
    }
    if let Some(output) = artifact.output.as_mut() {
        canonicalize_partition(output.partition.as_mut());
    }
}

/// Sorts every repeated field of a receipt into canonical order.
pub fn canonicalize_receipt(receipt: &mut TaskReceipt) {
    receipt.outputs.sort_by(|a, b| a.path.cmp(&b.path));
    for output in &mut receipt.outputs {
        canonicalize_output(output);
    }
}

fn canonicalize_delete(file: &mut DeleteFile) {
    file.equality_field_ids.sort_unstable();
    canonicalize_partition(file.partition.as_mut());
}

fn canonicalize_output(output: &mut OutputFile) {
    output.columns.sort_by_key(|column| column.field_id);
    output.split_offsets.sort_unstable();
    canonicalize_partition(output.partition.as_mut());
}

fn canonicalize_partition(partition: Option<&mut PartitionKey>) {
    if let Some(partition) = partition {
        partition.values.sort_by_key(|value| value.field_id);
    }
}

fn digest(domain: &[u8], message: &impl Message) -> Sha256Digest {
    domain_digest(domain, &message.encode_to_vec())
}

/// `SHA-256(domain || 0x00 || bytes)`.
pub(crate) fn domain_digest(domain: &[u8], bytes: &[u8]) -> Sha256Digest {
    Sha256Digest::of(domain, bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use prost_types::field_descriptor_proto::Type;
    use prost_types::{DescriptorProto, FileDescriptorSet};

    use super::*;
    use crate::FILE_DESCRIPTOR_SET;
    use crate::artifact::{
        Baseline, ColumnMetrics, DeleteContent, ExecutionMetrics, OutputSpec, PartitionValue,
        SourceFile, TableRef,
    };

    fn partition_key(values: &[(i32, Option<&[u8]>)]) -> PartitionKey {
        PartitionKey {
            spec_id: 0,
            values: values
                .iter()
                .map(|(field_id, value)| PartitionValue {
                    field_id: *field_id,
                    value: value.map(<[u8]>::to_vec),
                })
                .collect(),
        }
    }

    fn sample_artifact() -> TaskArtifact {
        TaskArtifact {
            job_id: "0199f000-0000-7000-8000-000000000001".to_owned(),
            plan_id: "0199f000-0000-7000-8000-000000000002".to_owned(),
            task_id: "0199f000-0000-7000-8000-000000000003".to_owned(),
            table: Some(TableRef {
                table_uuid: "0199f000-0000-7000-8000-0000000000aa".to_owned(),
                location: "s3://warehouse/db/events".to_owned(),
                format_version: 2,
            }),
            baseline: Some(Baseline {
                snapshot_id: 42,
                sequence_number: 7,
                schema_id: 1,
            }),
            source_files: vec![
                SourceFile {
                    path: "s3://warehouse/db/events/data/b.parquet".to_owned(),
                    file_size_bytes: 2048,
                    record_count: 20,
                    data_sequence_number: 6,
                    partition: Some(partition_key(&[(1001, Some(b"x")), (1000, None)])),
                },
                SourceFile {
                    path: "s3://warehouse/db/events/data/a.parquet".to_owned(),
                    file_size_bytes: 1024,
                    record_count: 10,
                    data_sequence_number: 5,
                    partition: Some(partition_key(&[(1000, None), (1001, Some(b"x"))])),
                },
            ],
            delete_files: vec![DeleteFile {
                path: "s3://warehouse/db/events/data/eq.parquet".to_owned(),
                file_size_bytes: 512,
                record_count: 3,
                data_sequence_number: 7,
                content: DeleteContent::Equality.into(),
                equality_field_ids: vec![3, 1],
                referenced_data_file: None,
                partition: Some(partition_key(&[(1000, None)])),
            }],
            output: Some(OutputSpec {
                partition_spec_id: 0,
                partition: Some(partition_key(&[(1001, Some(b"x")), (1000, None)])),
                target_file_size_bytes: 512 * 1024 * 1024,
                data_sequence_number: 7,
            }),
        }
    }

    fn sample_receipt() -> TaskReceipt {
        TaskReceipt {
            task_id: "0199f000-0000-7000-8000-000000000003".to_owned(),
            attempt_id: "0199f000-0000-7000-8000-000000000004".to_owned(),
            generation: 1,
            artifact_sha256: artifact_digest(&sample_artifact()).as_bytes().to_vec(),
            outputs: vec![OutputFile {
                path: "s3://warehouse/db/events/data/bergsmith/j/a/part-0.parquet".to_owned(),
                file_size_bytes: 3000,
                record_count: 27,
                partition: Some(partition_key(&[(1000, None)])),
                columns: vec![
                    ColumnMetrics {
                        field_id: 2,
                        column_size_bytes: 100,
                        value_count: 27,
                        null_value_count: 0,
                        nan_value_count: None,
                        lower_bound: Some(b"a".to_vec()),
                        upper_bound: Some(b"z".to_vec()),
                    },
                    ColumnMetrics {
                        field_id: 1,
                        column_size_bytes: 200,
                        value_count: 27,
                        null_value_count: 1,
                        nan_value_count: None,
                        lower_bound: None,
                        upper_bound: None,
                    },
                ],
                split_offsets: vec![4],
            }],
            metrics: Some(ExecutionMetrics {
                rows_read: 30,
                rows_deleted: 3,
                rows_written: 27,
                bytes_read: 3072,
                bytes_written: 3000,
            }),
        }
    }

    // Golden digests. If one of these changes, the wire format changed:
    // existing artifacts and receipts would no longer verify. Treat that as a
    // breaking change, not a test to update.
    const ARTIFACT_GOLDEN: &str =
        "74804af24cf761fbabb5ed063d723c3efc434658789832701fb52217c4d4c3db";
    const RECEIPT_GOLDEN: &str = "d01cc9ff627d6afe76c677f8d3d4d981270fef27e63309716b1f627b193d786f";

    #[test]
    fn artifact_digest_matches_the_golden_value() {
        assert_eq!(
            artifact_digest(&sample_artifact()).to_hex(),
            ARTIFACT_GOLDEN
        );
    }

    #[test]
    fn receipt_digest_matches_the_golden_value() {
        assert_eq!(receipt_digest(&sample_receipt()).to_hex(), RECEIPT_GOLDEN);
    }

    #[test]
    fn digests_ignore_the_order_of_repeated_fields() {
        let mut reordered = sample_artifact();
        reordered.source_files.reverse();
        canonicalize_artifact(&mut reordered);
        assert_eq!(
            artifact_digest(&reordered),
            artifact_digest(&sample_artifact())
        );

        let mut receipt = sample_receipt();
        receipt.outputs[0].columns.reverse();
        assert_eq!(receipt_digest(&receipt), receipt_digest(&sample_receipt()));
    }

    #[test]
    fn digests_change_with_any_field() {
        let mut changed = sample_artifact();
        changed.source_files[0].record_count += 1;
        assert_ne!(
            artifact_digest(&changed),
            artifact_digest(&sample_artifact())
        );

        let mut changed = sample_receipt();
        changed.generation = 2;
        assert_ne!(receipt_digest(&changed), receipt_digest(&sample_receipt()));
    }

    #[test]
    fn artifact_and_receipt_digests_are_domain_separated() {
        assert_ne!(
            artifact_digest(&TaskArtifact::default()),
            receipt_digest(&TaskReceipt::default())
        );
    }

    #[test]
    fn digests_round_trip_through_bytes_and_hex() {
        let digest = artifact_digest(&sample_artifact());
        let parsed = Sha256Digest::from_slice(digest.as_bytes()).unwrap();
        assert_eq!(parsed, digest);
        assert_eq!(digest.to_string().len(), 64);
        assert!(format!("{digest:?}").contains(&digest.to_hex()));
        assert_eq!(
            Sha256Digest::from_slice(&[0; 31]).unwrap_err().to_string(),
            "a SHA-256 digest has 32 bytes, got 31"
        );
    }

    /// Every message reachable from the hashed roots, by full name.
    fn reachable(set: &FileDescriptorSet, roots: &[&str]) -> Vec<DescriptorProto> {
        let mut by_name = std::collections::BTreeMap::new();
        for file in &set.file {
            for message in &file.message_type {
                by_name.insert(format!(".{}.{}", file.package(), message.name()), message);
            }
        }
        let mut seen = BTreeSet::new();
        let mut stack: Vec<String> = roots.iter().map(|r| (*r).to_owned()).collect();
        let mut found = Vec::new();
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let message = by_name
                .get(&name)
                .unwrap_or_else(|| panic!("unknown message {name}"));
            for field in &message.field {
                if field.r#type() == Type::Message {
                    stack.push(field.type_name().to_owned());
                }
            }
            found.push((*message).clone());
        }
        found
    }

    #[test]
    fn hashed_messages_have_no_maps_or_floating_point_fields() {
        let set = FileDescriptorSet::decode(FILE_DESCRIPTOR_SET).unwrap();
        let messages = reachable(
            &set,
            &[
                ".bergsmith.artifact.v1.TaskArtifact",
                ".bergsmith.artifact.v1.TaskReceipt",
            ],
        );

        assert!(messages.len() > 5);
        for message in messages {
            assert!(
                message.nested_type.is_empty(),
                "{} has a map",
                message.name()
            );
            for field in &message.field {
                assert!(
                    !matches!(field.r#type(), Type::Double | Type::Float),
                    "{}.{} is floating point",
                    message.name(),
                    field.name()
                );
            }
        }
    }
}
