//! The objects a table still references, proven from its metadata and the
//! publication ledger.
//!
//! Scanning is bounded by manifests, manifest entries, and object bytes. Avro
//! decoding runs on blocking tasks so a large scan never occupies an async
//! runtime thread.

use std::collections::BTreeSet;

use bergsmith_storage::{Backend, Location};
use iceberg::io::FileIO;
use iceberg::spec::{Manifest, ManifestList, TableMetadata};

use super::OrphanError;

/// The bounds of one reachability scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReachabilityBounds {
    manifests: usize,
    entries: usize,
    bytes: u64,
}

impl ReachabilityBounds {
    /// Builds bounds from an operation's configuration.
    ///
    /// # Errors
    ///
    /// [`OrphanError::InvalidPolicy`] when either bound is zero.
    pub fn new(
        max_manifests: usize,
        max_entries: usize,
        max_bytes: u64,
    ) -> Result<Self, OrphanError> {
        if max_manifests == 0 || max_entries == 0 || max_bytes == 0 {
            return Err(OrphanError::InvalidPolicy);
        }
        Ok(Self {
            manifests: max_manifests,
            entries: max_entries,
            bytes: max_bytes,
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

    /// The number of manifests read.
    #[must_use]
    pub const fn manifests(&self) -> usize {
        self.manifests
    }

    /// The number of manifest entries read.
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

    /// Reads one manifest list and everything it references.
    async fn scan_manifest_list(
        &mut self,
        manifest_list: &str,
        format_version: iceberg::spec::FormatVersion,
        file_io: &FileIO,
        bounds: ReachabilityBounds,
    ) -> Result<(), OrphanError> {
        self.insert(manifest_list)?;
        let input = file_io.new_input(manifest_list)?;
        let size = input.metadata().await?.size;
        if size > bounds.max_bytes() {
            return Err(OrphanError::ScanObjectTooLarge {
                size,
                limit: bounds.max_bytes(),
            });
        }
        let bytes = input.read().await?;
        // Avro decoding is CPU-bound; keep it off the runtime's async threads.
        let list = tokio::task::spawn_blocking(move || {
            ManifestList::parse_with_version(&bytes, format_version)
        })
        .await??;
        for manifest in list.entries() {
            self.manifests = self.manifests.saturating_add(1);
            if self.manifests > bounds.max_manifests() {
                return Err(OrphanError::ScanTooLarge {
                    manifests: self.manifests,
                    entries: self.entries,
                });
            }
            // The counts let a manifest be refused before it is decoded. An
            // older manifest without them cannot be bounded, so the scan
            // refuses it instead of decoding an unknown-size object.
            let (Some(added), Some(existing), Some(deleted)) = (
                manifest.added_files_count,
                manifest.existing_files_count,
                manifest.deleted_files_count,
            ) else {
                return Err(OrphanError::ScanUnboundedManifest {
                    path: manifest.manifest_path.clone(),
                });
            };
            let declared = added.saturating_add(existing).saturating_add(deleted);
            if self
                .entries
                .saturating_add(usize::try_from(declared).unwrap_or(usize::MAX))
                > bounds.max_entries()
            {
                return Err(OrphanError::ScanTooLarge {
                    manifests: self.manifests,
                    entries: self
                        .entries
                        .saturating_add(usize::try_from(declared).unwrap_or(usize::MAX)),
                });
            }
            self.insert(&manifest.manifest_path)?;
            // The declared length is only a hint. Stat the object so the read
            // that follows cannot exceed the byte bound.
            let input = file_io.new_input(&manifest.manifest_path)?;
            let size = input.metadata().await?.size;
            if size > bounds.max_bytes() {
                return Err(OrphanError::ScanObjectTooLarge {
                    size,
                    limit: bounds.max_bytes(),
                });
            }
            let bytes = file_io.new_input(&manifest.manifest_path)?.read().await?;
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

    pub(crate) fn insert(&mut self, path: &str) -> Result<(), OrphanError> {
        let location = Location::parse(path).map_err(OrphanError::Location)?;
        self.files.insert(ObjectIdentity::of(&location));
        Ok(())
    }
}
