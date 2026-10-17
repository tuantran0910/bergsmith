//! Which delete files apply to a data file, decided from metadata.
//!
//! The v2 rules: a delete applies when its partition equals the data
//! file's (same spec and values), or an equality delete's spec is unpartitioned.
//! A position delete applies when the data sequence number is <= the delete's,
//! an equality delete when it is strictly less. A position delete that names its data
//! file, or whose `file_path` bounds exclude a path, does not apply to other
//! files. Including a delete that turns out not to match is safe: the
//! executor matches deletes by path and key.

use std::collections::BTreeMap;

use crate::inventory::{DeleteEntry, DeleteKind, FileEntry};
use crate::partition::{BoundSpec, PartitionKey, Specs};

pub(crate) struct DeleteIndex<'a> {
    /// Equality deletes of unpartitioned specs, which may apply to any file.
    global: Vec<&'a DeleteEntry>,
    by_partition: BTreeMap<&'a PartitionKey, Vec<&'a DeleteEntry>>,
}

impl<'a> DeleteIndex<'a> {
    pub(crate) fn new(deletes: &'a [DeleteEntry], specs: &Specs) -> Self {
        let mut index = Self {
            global: Vec::new(),
            by_partition: BTreeMap::new(),
        };
        for delete in deletes {
            let unpartitioned = matches!(delete.kind, DeleteKind::Equality { .. })
                && specs
                    .get(delete.file.partition.spec_id())
                    .is_ok_and(BoundSpec::is_unpartitioned);
            if unpartitioned {
                index.global.push(delete);
            } else {
                index
                    .by_partition
                    .entry(&delete.file.partition)
                    .or_default()
                    .push(delete);
            }
        }
        index
    }

    /// The deletes that may apply to `data`, sorted by path.
    pub(crate) fn applicable(&self, data: &FileEntry) -> Vec<&'a DeleteEntry> {
        let local = self.by_partition.get(&data.partition).into_iter().flatten();
        let mut applicable: Vec<&DeleteEntry> = self
            .global
            .iter()
            .chain(local)
            .copied()
            .filter(|delete| applies(delete, data))
            .collect();
        applicable.sort_by(|a, b| a.file.path.cmp(&b.file.path));
        applicable.dedup_by(|a, b| a.file.path == b.file.path);
        applicable
    }
}

fn applies(delete: &DeleteEntry, data: &FileEntry) -> bool {
    match &delete.kind {
        DeleteKind::Equality { .. } => delete.file.sequence_number > data.sequence_number,
        DeleteKind::Position {
            referenced,
            path_bounds,
        } => {
            delete.file.sequence_number >= data.sequence_number
                && referenced.as_ref().is_none_or(|path| *path == data.path)
                && path_bounds.as_ref().is_none_or(|(lower, upper)| {
                    lower.as_str() <= data.path.as_str() && data.path.as_str() <= upper.as_str()
                })
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use iceberg::spec::DataFileFormat;

    use super::*;
    use crate::partition::PartitionKey;

    pub(crate) fn file(path: &str, sequence_number: i64, partition: PartitionKey) -> FileEntry {
        FileEntry {
            path: path.to_owned(),
            format: DataFileFormat::Parquet,
            size: 10,
            records: 1,
            sequence_number,
            partition,
        }
    }

    fn position(path: &str, sequence_number: i64, kind: DeleteKind) -> DeleteEntry {
        DeleteEntry {
            file: file(path, sequence_number, PartitionKey::unpartitioned()),
            kind,
        }
    }

    fn untargeted() -> DeleteKind {
        DeleteKind::Position {
            referenced: None,
            path_bounds: None,
        }
    }

    fn applies_to(delete: &DeleteEntry, data: &str, sequence_number: i64) -> bool {
        applies(
            delete,
            &file(data, sequence_number, PartitionKey::unpartitioned()),
        )
    }

    #[test]
    fn position_deletes_apply_up_to_their_own_sequence_number() {
        let delete = position("p", 2, untargeted());

        assert!(applies_to(&delete, "d", 1));
        assert!(applies_to(&delete, "d", 2));
        assert!(!applies_to(&delete, "d", 3));
    }

    #[test]
    fn equality_deletes_apply_only_to_older_data() {
        let delete = position("e", 2, DeleteKind::Equality { field_ids: vec![1] });

        assert!(applies_to(&delete, "d", 1));
        assert!(!applies_to(&delete, "d", 2));
    }

    #[test]
    fn a_referenced_data_file_scopes_a_position_delete() {
        let delete = position(
            "p",
            2,
            DeleteKind::Position {
                referenced: Some("s3://b/a".into()),
                path_bounds: None,
            },
        );

        assert!(applies_to(&delete, "s3://b/a", 1));
        assert!(!applies_to(&delete, "s3://b/ab", 1));
    }

    #[test]
    fn path_bounds_scope_a_position_delete() {
        let delete = position(
            "p",
            2,
            DeleteKind::Position {
                referenced: None,
                path_bounds: Some(("s3://b/c".into(), "s3://b/e".into())),
            },
        );

        for (path, expected) in [
            ("s3://b/b", false),
            ("s3://b/c", true),
            ("s3://b/d1", true),
            ("s3://b/e", true),
            ("s3://b/e1", false),
        ] {
            assert_eq!(applies_to(&delete, path, 1), expected, "{path}");
        }
    }
}
