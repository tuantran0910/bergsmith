//! Which delete files apply to a data file, decided from metadata.
//!
//! The v2 rules: a delete applies when its partition equals the data
//! file's (same spec and values), or an equality delete's spec is unpartitioned.
//! A position delete applies when the data sequence number is <= the delete's,
//! an equality delete when it is strictly less.
//!
//! Recorded metrics then exclude deletes that provably match no row, exactly
//! as the pinned iceberg-rust scan planner does (ADR 0027), so the executor
//! can require the reader's attached deletes to equal this index's:
//!
//! - A position delete with a `referenced_data_file` applies only to that
//!   file. Otherwise, valid `file_path` bounds that exclude the data path
//!   exclude it; reversed bounds prove nothing.
//! - An equality delete is excluded when, for some key field, a match is
//!   impossible: the files cannot both hold a null there, and the data and
//!   delete bounds are disjoint. Null equals null, so a field where both may
//!   hold nulls, or with any bound unknown or of different types, proves
//!   nothing.
//!
//! Exclusions rely only on metrics the planned manifests record. Missing
//! metrics fail open: the delete is kept, and the executor's reader applies
//! it by path and key.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crate::inventory::{DeleteEntry, DeleteKind, FileEntry};
use crate::partition::{BoundSpec, PartitionKey, Specs};

pub(crate) struct DeleteIndex<'a> {
    /// Equality deletes of unpartitioned specs, which may apply to any file.
    /// The flag marks a spec with fields, all of them void transforms.
    global: Vec<(&'a DeleteEntry, bool)>,
    by_partition: BTreeMap<&'a PartitionKey, Vec<&'a DeleteEntry>>,
}

impl<'a> DeleteIndex<'a> {
    pub(crate) fn new(deletes: &'a [DeleteEntry], specs: &Specs) -> Self {
        let mut index = Self {
            global: Vec::new(),
            by_partition: BTreeMap::new(),
        };
        for delete in deletes {
            let spec = specs.get(delete.file.partition.spec_id()).ok();
            let unpartitioned = matches!(delete.kind, DeleteKind::Equality { .. })
                && spec.is_some_and(BoundSpec::is_unpartitioned);
            if unpartitioned {
                let void = spec.is_some_and(|spec| !spec.spec.fields().is_empty());
                index.global.push((delete, void));
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
            .map(|(delete, _)| *delete)
            .chain(local.copied())
            .filter(|delete| applies(delete, data))
            .collect();
        applicable.sort_by(|a, b| a.file.path.cmp(&b.file.path));
        applicable.dedup_by(|a, b| a.file.path == b.file.path);
        applicable
    }

    /// Applicable equality deletes the native reader would not attach to `data`.
    ///
    /// Iceberg applies an equality delete in a spec whose fields are all void
    /// transforms globally, like Java's `PartitionSpec::isUnpartitioned`. The
    /// pinned iceberg-rust scan planner treats only a spec without fields as
    /// global, and scopes a void spec's deletes to data in that same spec.
    pub(crate) fn unattached_void_deletes(&self, data: &FileEntry) -> Vec<&'a DeleteEntry> {
        self.global
            .iter()
            .filter(|(delete, void)| {
                *void
                    && delete.file.partition.spec_id() != data.partition.spec_id()
                    && applies(delete, data)
            })
            .map(|(delete, _)| *delete)
            .collect()
    }
}

/// Deletes not yet shown to apply to any data file, scoped as in
/// [`DeleteIndex`].
///
/// Each delete is tested only until it first applies, so classifying an
/// inventory costs at most one test per pending delete in scope per data
/// file, and nothing once every delete is accounted for.
pub(crate) struct PendingDeletes<'a> {
    global: Vec<&'a DeleteEntry>,
    by_partition: BTreeMap<&'a PartitionKey, Vec<&'a DeleteEntry>>,
    applied: BTreeSet<&'a str>,
}

impl<'a> PendingDeletes<'a> {
    pub(crate) fn new(deletes: &'a [DeleteEntry], specs: &Specs) -> Self {
        let index = DeleteIndex::new(deletes, specs);
        Self {
            global: index.global.into_iter().map(|(delete, _)| delete).collect(),
            by_partition: index.by_partition,
            applied: BTreeSet::new(),
        }
    }

    /// Records the pending deletes that apply to `data`; returns how many
    /// deletes it tested.
    pub(crate) fn discharge(&mut self, data: &FileEntry) -> usize {
        let applied = &mut self.applied;
        let mut tested = self.global.len();
        self.global.retain(|delete| {
            let live = applies(delete, data);
            if live {
                applied.insert(delete.file.path.as_str());
            }
            !live
        });
        if let Some(local) = self.by_partition.get_mut(&data.partition) {
            tested += local.len();
            local.retain(|delete| {
                let live = applies(delete, data);
                if live {
                    applied.insert(delete.file.path.as_str());
                }
                !live
            });
            if local.is_empty() {
                self.by_partition.remove(&data.partition);
            }
        }
        tested
    }

    /// Whether every delete has been shown to apply.
    pub(crate) fn is_empty(&self) -> bool {
        self.global.is_empty() && self.by_partition.is_empty()
    }

    /// The paths of the deletes shown to apply to some data file.
    pub(crate) fn into_applied(self) -> BTreeSet<&'a str> {
        self.applied
    }
}

fn applies(delete: &DeleteEntry, data: &FileEntry) -> bool {
    match &delete.kind {
        DeleteKind::Equality { field_ids } => {
            delete.file.sequence_number > data.sequence_number
                && may_match(field_ids, &delete.file, data)
        }
        DeleteKind::Position {
            referenced,
            path_bounds,
        } => {
            delete.file.sequence_number >= data.sequence_number
                && match referenced {
                    Some(path) => *path == data.path,
                    None => path_bounds.as_ref().is_none_or(|(lower, upper)| {
                        lower > upper
                            || (lower.as_str() <= data.path.as_str()
                                && data.path.as_str() <= upper.as_str())
                    }),
                }
        }
    }
}

/// Whether recorded metrics allow a row of `data` to equal a key of `delete`.
fn may_match(field_ids: &[i32], delete: &FileEntry, data: &FileEntry) -> bool {
    field_ids.iter().all(|&field_id| {
        let (data, delete) = (data.metrics(field_id), delete.metrics(field_id));
        if data.may_contain_null() && delete.may_contain_null() {
            return true;
        }
        let (Some(data_lower), Some(data_upper), Some(delete_lower), Some(delete_upper)) =
            (&data.lower, &data.upper, &delete.lower, &delete.upper)
        else {
            return true;
        };
        data_lower.partial_cmp(delete_upper) != Some(Ordering::Greater)
            && delete_lower.partial_cmp(data_upper) != Some(Ordering::Greater)
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use iceberg::spec::{DataFileFormat, Datum};

    use super::*;
    use crate::inventory::KeyMetrics;
    use crate::partition::PartitionKey;

    pub(crate) fn file(path: &str, sequence_number: i64, partition: PartitionKey) -> FileEntry {
        FileEntry {
            path: path.to_owned(),
            format: DataFileFormat::Parquet,
            size: 10,
            records: 1,
            sequence_number,
            partition,
            key_metrics: BTreeMap::new(),
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
    fn a_referenced_data_file_scopes_a_position_delete_and_overrides_bounds() {
        let delete = position(
            "p",
            2,
            DeleteKind::Position {
                referenced: Some("s3://b/a".into()),
                path_bounds: Some(("s3://b/x".into(), "s3://b/y".into())),
            },
        );

        assert!(applies_to(&delete, "s3://b/a", 1));
        assert!(!applies_to(&delete, "s3://b/ab", 1));
        assert!(!applies_to(&delete, "s3://b/x1", 1));
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

    #[test]
    fn reversed_path_bounds_prove_nothing() {
        let delete = position(
            "p",
            2,
            DeleteKind::Position {
                referenced: None,
                path_bounds: Some(("s3://b/e".into(), "s3://b/c".into())),
            },
        );

        assert!(applies_to(&delete, "s3://b/a", 1));
    }

    fn metrics(nulls: Option<u64>, bounds: Option<(Datum, Datum)>) -> KeyMetrics {
        let (lower, upper) = bounds.unzip();
        KeyMetrics {
            nulls,
            lower,
            upper,
        }
    }

    fn with(mut entry: FileEntry, field_id: i32, metrics: KeyMetrics) -> FileEntry {
        entry.key_metrics.insert(field_id, metrics);
        entry
    }

    fn longs(lower: i64, upper: i64) -> (Datum, Datum) {
        (Datum::long(lower), Datum::long(upper))
    }

    /// Each case: data metrics, delete metrics, whether the delete may apply.
    #[test]
    fn equality_metrics_exclude_only_provably_unmatched_deletes() {
        let cases = [
            // Disjoint bounds without nulls prove no match, in either order.
            (
                metrics(Some(0), Some(longs(1, 3))),
                metrics(Some(0), Some(longs(10, 12))),
                false,
            ),
            (
                metrics(Some(0), Some(longs(10, 12))),
                metrics(Some(0), Some(longs(1, 3))),
                false,
            ),
            // Touching or overlapping bounds may match.
            (
                metrics(Some(0), Some(longs(1, 10))),
                metrics(Some(0), Some(longs(10, 12))),
                true,
            ),
            // Both may hold a null, which matches a null key.
            (
                metrics(Some(2), Some(longs(1, 3))),
                metrics(Some(1), Some(longs(10, 12))),
                true,
            ),
            (
                metrics(None, Some(longs(1, 3))),
                metrics(None, Some(longs(10, 12))),
                true,
            ),
            // Only one side may hold a null, so the bounds decide.
            (
                metrics(Some(2), Some(longs(1, 3))),
                metrics(Some(0), Some(longs(10, 12))),
                false,
            ),
            (
                metrics(None, Some(longs(1, 3))),
                metrics(Some(0), Some(longs(10, 12))),
                false,
            ),
            // A missing bound or a type mismatch proves nothing.
            (
                metrics(Some(0), None),
                metrics(Some(0), Some(longs(10, 12))),
                true,
            ),
            (
                metrics(Some(0), Some(longs(1, 3))),
                metrics(Some(0), None),
                true,
            ),
            (
                metrics(Some(0), Some((Datum::int(1), Datum::int(3)))),
                metrics(Some(0), Some(longs(10, 12))),
                true,
            ),
            // An all-null delete column has no bounds.
            (
                metrics(Some(0), Some(longs(1, 3))),
                metrics(Some(4), None),
                true,
            ),
        ];
        for (index, (data, delete, expected)) in cases.into_iter().enumerate() {
            let delete = DeleteEntry {
                file: with(file("e", 2, PartitionKey::unpartitioned()), 1, delete),
                kind: DeleteKind::Equality { field_ids: vec![1] },
            };
            let data = with(file("d", 1, PartitionKey::unpartitioned()), 1, data);
            assert_eq!(applies(&delete, &data), expected, "case {index}");
        }
    }

    #[test]
    fn one_disjoint_field_of_a_composite_key_excludes_the_delete() {
        let delete = DeleteEntry {
            file: with(
                with(
                    file("e", 2, PartitionKey::unpartitioned()),
                    1,
                    metrics(None, Some(longs(1, 3))),
                ),
                2,
                metrics(Some(0), Some(longs(10, 12))),
            ),
            kind: DeleteKind::Equality {
                field_ids: vec![1, 2],
            },
        };
        let data = with(
            with(
                file("d", 1, PartitionKey::unpartitioned()),
                1,
                metrics(None, Some(longs(1, 3))),
            ),
            2,
            metrics(Some(0), Some(longs(20, 30))),
        );
        assert!(!applies(&delete, &data));
        let keyless = DeleteEntry {
            kind: DeleteKind::Equality { field_ids: vec![] },
            ..delete
        };
        assert!(applies(&keyless, &data));
    }
}
