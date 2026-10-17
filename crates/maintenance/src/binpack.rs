//! Grouping and first-fit-decreasing binpacking (BS-009.T3).
//!
//! Files are grouped by `(spec_id, partition)`, so a task never mixes
//! partitions or specs and replacement files keep their original spec.
//! Groups follow the order of their keys. Within a group, files sort by size
//! descending, then path, and each goes into the first bin with room. The
//! order is total, so the same candidates always give the same bins.

use std::collections::BTreeMap;

use crate::partition::PartitionKey;

/// A file to pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Item<'a> {
    /// The caller's identifier, returned in [`Bin::items`].
    pub(crate) id: usize,
    pub(crate) key: &'a PartitionKey,
    pub(crate) path: &'a str,
    pub(crate) size: u64,
}

/// Files of one group whose sizes add up to at most the capacity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bin<'a> {
    pub(crate) key: &'a PartitionKey,
    /// Position among the group's bins, from 0.
    pub(crate) ordinal: usize,
    /// Item identifiers, in packing order.
    pub(crate) items: Vec<usize>,
    pub(crate) bytes: u64,
}

/// Packs `items` into bins of at most `capacity` bytes, in canonical order.
/// An item larger than `capacity` gets a bin of its own.
pub(crate) fn first_fit_decreasing<'a>(items: &[Item<'a>], capacity: u64) -> Vec<Bin<'a>> {
    let mut groups: BTreeMap<&PartitionKey, Vec<&Item<'a>>> = BTreeMap::new();
    for item in items {
        groups.entry(item.key).or_default().push(item);
    }
    let mut bins = Vec::new();
    for (key, mut group) in groups {
        group.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(b.path)));
        let mut packed: Vec<Bin<'a>> = Vec::new();
        for item in group {
            let fits = packed
                .iter_mut()
                .find(|bin| bin.bytes.saturating_add(item.size) <= capacity);
            match fits {
                Some(bin) => {
                    bin.items.push(item.id);
                    bin.bytes += item.size;
                }
                None => packed.push(Bin {
                    key,
                    ordinal: packed.len(),
                    items: vec![item.id],
                    bytes: item.size,
                }),
            }
        }
        bins.extend(packed);
    }
    bins
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(spec_id: i32) -> PartitionKey {
        let mut key = PartitionKey::unpartitioned();
        key.set_spec_id(spec_id);
        key
    }

    fn items<'a>(key: &'a PartitionKey, sizes: &[u64], paths: &'a [String]) -> Vec<Item<'a>> {
        sizes
            .iter()
            .zip(paths)
            .enumerate()
            .map(|(id, (&size, path))| Item {
                id,
                key,
                path,
                size,
            })
            .collect()
    }

    fn paths(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("f{index}")).collect()
    }

    #[test]
    fn first_fit_decreasing_fills_earlier_bins_first() {
        let key = key(0);
        let paths = paths(6);
        let sizes = [4, 8, 1, 4, 2, 1];

        let bins = first_fit_decreasing(&items(&key, &sizes, &paths), 10);

        let contents: Vec<(Vec<usize>, u64)> = bins
            .iter()
            .map(|bin| (bin.items.clone(), bin.bytes))
            .collect();
        // Sorted: 8 (f1), 4 (f0), 4 (f3), 2 (f4), 1 (f2), 1 (f5).
        assert_eq!(contents, [(vec![1, 4], 10), (vec![0, 3, 2, 5], 10)]);
        assert_eq!(
            bins.iter().map(|bin| bin.ordinal).collect::<Vec<_>>(),
            [0, 1]
        );
    }

    #[test]
    fn equal_sizes_are_ordered_by_path() {
        let key = key(0);
        let paths = vec!["b".to_owned(), "a".to_owned(), "c".to_owned()];

        let bins = first_fit_decreasing(&items(&key, &[5, 5, 5], &paths), 10);

        assert_eq!(bins[0].items, [1, 0]);
        assert_eq!(bins[1].items, [2]);
    }

    #[test]
    fn groups_never_share_a_bin_and_follow_key_order() {
        let (first, second) = (key(0), key(1));
        let paths = paths(4);
        let mut all = items(&second, &[1, 1], &paths[..2]);
        all.extend(
            items(&first, &[1, 1], &paths[2..])
                .into_iter()
                .map(|item| Item {
                    id: item.id + 2,
                    ..item
                }),
        );

        let bins = first_fit_decreasing(&all, 100);

        assert_eq!(bins.len(), 2);
        assert_eq!(
            (bins[0].key, bins[0].items.as_slice()),
            (&first, &[2, 3][..])
        );
        assert_eq!(
            (bins[1].key, bins[1].items.as_slice()),
            (&second, &[0, 1][..])
        );
    }

    #[test]
    fn an_oversized_item_gets_its_own_bin() {
        let key = key(0);
        let paths = paths(2);

        let bins = first_fit_decreasing(&items(&key, &[25, 3], &paths), 10);

        assert_eq!(bins.len(), 2);
        assert_eq!(bins[0].bytes, 25);
    }
}
