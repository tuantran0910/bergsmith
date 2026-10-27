//! The objects a table still references, proven from its metadata and the
//! publication ledger.
//!
//! Scanning is bounded by manifests, manifest entries, and object bytes. Avro
//! decoding runs on blocking tasks so a large scan never occupies an async
//! runtime thread.

use std::collections::BTreeSet;
use std::time::Duration;

use bergsmith_storage::{Backend, Location};
use iceberg::io::{FileIO, FileRead};
use iceberg::spec::{Manifest, ManifestFile, ManifestList, TableMetadata};

use super::OrphanError;

/// The bounds of one reachability scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReachabilityBounds {
    manifests: usize,
    entries: usize,
    bytes: u64,
    read_timeout: Duration,
}

impl ReachabilityBounds {
    /// Builds bounds from an operation's configuration.
    ///
    /// # Errors
    ///
    /// [`OrphanError::InvalidPolicy`] when any bound is zero.
    pub fn new(
        max_manifests: usize,
        max_entries: usize,
        max_bytes: u64,
        read_timeout: Duration,
    ) -> Result<Self, OrphanError> {
        if max_manifests == 0 || max_entries == 0 || max_bytes == 0 || read_timeout.is_zero() {
            return Err(OrphanError::InvalidPolicy);
        }
        Ok(Self {
            manifests: max_manifests,
            entries: max_entries,
            bytes: max_bytes,
            read_timeout,
        })
    }

    /// The most manifests one scan reads.
    #[must_use]
    pub const fn max_manifests(&self) -> usize {
        self.manifests
    }

    /// The most manifest entries one scan reads.
    #[must_use]
    pub const fn max_entries(&self) -> usize {
        self.entries
    }

    /// The largest manifest list or manifest one scan reads, in bytes.
    #[must_use]
    pub const fn max_bytes(&self) -> u64 {
        self.bytes
    }

    /// The deadline for reading one manifest list or manifest.
    #[must_use]
    pub const fn read_timeout(&self) -> Duration {
        self.read_timeout
    }
}

/// The identity of one stored object, independent of its URI spelling.
///
/// `s3` and `s3a` name the same object, and so do other scheme aliases for one
/// backend. Reachability stores identities, so a live file written as `s3a`
/// cannot be missed because the listing spells it `s3`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ObjectIdentity(Backend, String, String);

impl ObjectIdentity {
    pub(crate) fn of(location: &Location) -> Self {
        Self(
            location.backend(),
            location.authority().to_owned(),
            location.key().to_owned(),
        )
    }
}

/// Every object the table or an unresolved publication still references.
///
/// The set is built from the current metadata and from the manifest lists
/// named by publication intents that may still be sent. Metadata files named
/// by `metadata-log` are protected, but the files those older versions
/// reference are not: this release cannot parse an older metadata file, so it
/// matches Iceberg's own orphan-file action, which also protects only what the
/// current metadata retains.
#[derive(Clone, Debug, Default)]
pub struct Reachable {
    files: BTreeSet<ObjectIdentity>,
    /// Manifest lists and manifests already read, so shared ones are read once.
    scanned: BTreeSet<ObjectIdentity>,
    manifests: usize,
    entries: usize,
}

impl Reachable {
    /// Whether `location` is referenced by the table.
    #[must_use]
    pub fn contains(&self, location: &Location) -> bool {
        self.files.contains(&ObjectIdentity::of(location))
    }

    /// The number of referenced objects.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether nothing is referenced.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The number of distinct manifests read.
    #[must_use]
    pub const fn manifests(&self) -> usize {
        self.manifests
    }

    /// The number of manifest entries read, counting each manifest once.
    #[must_use]
    pub const fn entries(&self) -> usize {
        self.entries
    }

    /// Captures the referenced objects of `metadata` and its manifests.
    ///
    /// `metadata_location` is the file the catalog returned; it is protected
    /// together with every metadata file named by `metadata-log`.
    ///
    /// # Errors
    ///
    /// [`OrphanError::Location`] for a malformed path in metadata,
    /// [`OrphanError::Io`] when reading a manifest list or manifest fails,
    /// [`OrphanError::Blocking`] when a decode task cannot complete,
    /// [`OrphanError::TimedOut`] when one read misses the read deadline,
    /// [`OrphanError::ScanUnboundedManifest`] when a manifest declares no entry
    /// counts, and [`OrphanError::ScanTooLarge`] or
    /// [`OrphanError::ScanObjectTooLarge`] when `bounds` is exceeded.
    pub async fn capture(
        metadata: &TableMetadata,
        metadata_location: &str,
        unresolved_manifest_lists: &[String],
        file_io: &FileIO,
        bounds: ReachabilityBounds,
    ) -> Result<Self, OrphanError> {
        let mut reachable = Self::default();
        reachable.insert(metadata_location)?;
        for log in metadata.metadata_log() {
            reachable.insert(&log.metadata_file)?;
        }
        for statistics in metadata.statistics_iter() {
            reachable.insert(&statistics.statistics_path)?;
        }
        for statistics in metadata.partition_statistics_iter() {
            reachable.insert(&statistics.statistics_path)?;
        }
        for snapshot in metadata.snapshots() {
            reachable
                .scan_manifest_list(
                    snapshot.manifest_list(),
                    metadata.format_version(),
                    file_io,
                    bounds,
                )
                .await?;
        }
        // A publication that never landed still owns the manifests its
        // persisted intent names; they live under the metadata directory and
        // nothing in the current snapshot references them.
        for manifest_list in unresolved_manifest_lists {
            reachable
                .scan_manifest_list(manifest_list, metadata.format_version(), file_io, bounds)
                .await?;
        }
        Ok(reachable)
    }

    /// Reads one manifest list and every manifest it names that this scan
    /// has not read yet.
    ///
    /// Snapshots carry most earlier manifests forward, so a manifest is read
    /// and counted once per scan, however many manifest lists name it. A
    /// manifest list already read (two intents naming one list) is skipped.
    async fn scan_manifest_list(
        &mut self,
        manifest_list: &str,
        format_version: iceberg::spec::FormatVersion,
        file_io: &FileIO,
        bounds: ReachabilityBounds,
    ) -> Result<(), OrphanError> {
        if !self.mark_scanned(manifest_list)? {
            return Ok(());
        }
        let bytes = read_bounded(file_io, manifest_list, bounds).await?;
        // Avro decoding is CPU-bound; keep it off the runtime's async threads.
        let list = tokio::task::spawn_blocking(move || {
            ManifestList::parse_with_version(&bytes, format_version)
        })
        .await??;
        for manifest in list.entries() {
            if !self.mark_scanned(&manifest.manifest_path)? {
                continue;
            }
            self.manifests = self.manifests.saturating_add(1);
            if self.manifests > bounds.max_manifests() {
                return Err(OrphanError::ScanTooLarge {
                    manifests: self.manifests,
                    entries: self.entries,
                });
            }
            let declared = declared_entries(manifest)?;
            let projected = self.entries.saturating_add(declared);
            if projected > bounds.max_entries() {
                return Err(OrphanError::ScanTooLarge {
                    manifests: self.manifests,
                    entries: projected,
                });
            }
            let bytes = read_bounded(file_io, &manifest.manifest_path, bounds).await?;
            let loaded =
                tokio::task::spawn_blocking(move || Manifest::parse_avro(&bytes)).await??;
            for entry in loaded.entries() {
                self.entries = self.entries.saturating_add(1);
                if self.entries > bounds.max_entries() {
                    return Err(OrphanError::ScanTooLarge {
                        manifests: self.manifests,
                        entries: self.entries,
                    });
                }
                self.insert(entry.data_file().file_path())?;
            }
        }
        Ok(())
    }

    /// Protects `path` and records it as read; `false` when it was already
    /// read by this scan.
    fn mark_scanned(&mut self, path: &str) -> Result<bool, OrphanError> {
        let location = Location::parse(path).map_err(OrphanError::Location)?;
        let identity = ObjectIdentity::of(&location);
        self.files.insert(identity.clone());
        Ok(self.scanned.insert(identity))
    }

    pub(crate) fn insert(&mut self, path: &str) -> Result<(), OrphanError> {
        let location = Location::parse(path).map_err(OrphanError::Location)?;
        self.files.insert(ObjectIdentity::of(&location));
        Ok(())
    }
}

/// The entry count a manifest declares, so it can be refused before decoding.
///
/// An older manifest without the counts cannot be bounded, so the scan refuses
/// it instead of decoding an object of unknown size.
fn declared_entries(manifest: &ManifestFile) -> Result<usize, OrphanError> {
    let (Some(added), Some(existing), Some(deleted)) = (
        manifest.added_files_count,
        manifest.existing_files_count,
        manifest.deleted_files_count,
    ) else {
        return Err(OrphanError::ScanUnboundedManifest {
            path: manifest.manifest_path.clone(),
        });
    };
    Ok(
        usize::try_from(added.saturating_add(existing).saturating_add(deleted))
            .unwrap_or(usize::MAX),
    )
}

/// Reads a whole manifest list or manifest, refusing one above the byte bound.
///
/// The declared length in a manifest list is only a hint, so the object is
/// stat-ed first. The read is then limited to the stat-ed size, so an object
/// replaced between the two calls cannot push the read past the bound; a
/// truncated read fails to decode instead. The stat and the read together
/// must finish within the bounds' read deadline.
async fn read_bounded(
    file_io: &FileIO,
    path: &str,
    bounds: ReachabilityBounds,
) -> Result<bytes::Bytes, OrphanError> {
    let read = async {
        let input = file_io.new_input(path)?;
        let size = input.metadata().await?.size;
        if size > bounds.max_bytes() {
            return Err(OrphanError::ScanObjectTooLarge {
                size,
                limit: bounds.max_bytes(),
            });
        }
        Ok(input.reader().await?.read(0..size).await?)
    };
    tokio::time::timeout(bounds.read_timeout(), read)
        .await
        .map_err(|_| OrphanError::TimedOut {
            step: super::OrphanStep::Scan,
        })?
}

#[cfg(test)]
mod tests {
    use iceberg::spec::ManifestContentType;

    use super::*;

    fn manifest(counts: [Option<u32>; 3]) -> ManifestFile {
        ManifestFile {
            manifest_path: "s3://b/t/metadata/m.avro".to_owned(),
            manifest_length: 1,
            partition_spec_id: 0,
            content: ManifestContentType::Data,
            sequence_number: 1,
            min_sequence_number: 1,
            added_snapshot_id: 1,
            added_files_count: counts[0],
            existing_files_count: counts[1],
            deleted_files_count: counts[2],
            added_rows_count: None,
            existing_rows_count: None,
            deleted_rows_count: None,
            partitions: None,
            key_metadata: None,
            first_row_id: None,
        }
    }

    #[test]
    fn a_manifest_without_declared_counts_is_refused() {
        for counts in [
            [None, Some(1), Some(1)],
            [Some(1), None, Some(1)],
            [Some(1), Some(1), None],
        ] {
            assert!(matches!(
                declared_entries(&manifest(counts)),
                Err(OrphanError::ScanUnboundedManifest { path }) if path.ends_with("m.avro")
            ));
        }
        assert_eq!(
            declared_entries(&manifest([Some(2), Some(3), Some(4)])).unwrap(),
            9
        );
        assert_eq!(
            declared_entries(&manifest([Some(u32::MAX), Some(u32::MAX), Some(1)])).unwrap(),
            usize::try_from(u32::MAX).unwrap()
        );
    }
}
