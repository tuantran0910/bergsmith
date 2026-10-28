//! The objects a table still references, proven from its metadata and the
//! publication ledger.
//!
//! Scanning is bounded by manifests, manifest entries, and object bytes. Avro
//! decoding runs on blocking tasks so a large scan never occupies an async
//! runtime thread.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
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
    lists: usize,
    total_bytes: u64,
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
            lists: usize::MAX,
            total_bytes: u64::MAX,
        })
    }

    /// Adds whole-scan bounds for reporting. Existing cleanup bounds are unchanged.
    ///
    /// # Errors
    /// Returns [`OrphanError::InvalidPolicy`] for a zero bound.
    pub fn with_scan_limits(mut self, lists: usize, total_bytes: u64) -> Result<Self, OrphanError> {
        if lists == 0 || total_bytes == 0 {
            return Err(OrphanError::InvalidPolicy);
        }
        self.lists = lists;
        self.total_bytes = total_bytes;
        Ok(self)
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
    pub(crate) fn is_within(&self, root: &Location) -> bool {
        self.0 == root.backend()
            && self.1 == root.authority()
            && (root.key().is_empty()
                || self.2 == root.key()
                || self
                    .2
                    .strip_prefix(root.key())
                    .is_some_and(|tail| tail.starts_with('/')))
    }

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
    lists: usize,
    bytes: u64,
    pub(crate) accounting: Option<References>,
    decoder: Option<Arc<tokio::sync::Semaphore>>,
}

/// Optional reporting view; deletion continues to use the conservative union.
#[derive(Clone, Debug, Default)]
pub(crate) struct References {
    pub(crate) metadata: BTreeSet<ObjectIdentity>,
    pub(crate) data: BTreeMap<ObjectIdentity, (u64, bool)>,
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
        Self::capture_inner(
            metadata,
            metadata_location,
            unresolved_manifest_lists,
            file_io,
            bounds,
            None,
        )
        .await
    }

    pub(crate) async fn capture_accounted(
        metadata: &TableMetadata,
        metadata_location: &str,
        unresolved_manifest_lists: &[String],
        file_io: &FileIO,
        bounds: ReachabilityBounds,
        decoder: Arc<tokio::sync::Semaphore>,
    ) -> Result<Self, OrphanError> {
        Self::capture_inner(
            metadata,
            metadata_location,
            unresolved_manifest_lists,
            file_io,
            bounds,
            Some(decoder),
        )
        .await
    }

    async fn capture_inner(
        metadata: &TableMetadata,
        metadata_location: &str,
        unresolved_manifest_lists: &[String],
        file_io: &FileIO,
        bounds: ReachabilityBounds,
        decoder: Option<Arc<tokio::sync::Semaphore>>,
    ) -> Result<Self, OrphanError> {
        let mut reachable = Self {
            accounting: decoder.as_ref().map(|_| References::default()),
            decoder,
            ..Self::default()
        };
        reachable.metadata(metadata_location)?;
        for log in metadata.metadata_log() {
            reachable.metadata(&log.metadata_file)?;
        }
        for statistics in metadata.statistics_iter() {
            reachable.metadata(&statistics.statistics_path)?;
        }
        for statistics in metadata.partition_statistics_iter() {
            reachable.metadata(&statistics.statistics_path)?;
        }
        // Main first: any shared manifest is classified as live before retained history.
        let main = metadata.current_snapshot();
        for snapshot in main
            .into_iter()
            .chain(metadata.snapshots().filter(|snapshot| {
                main.is_none_or(|head| snapshot.snapshot_id() != head.snapshot_id())
            }))
        {
            reachable
                .scan_manifest_list(
                    snapshot.manifest_list(),
                    metadata.format_version(),
                    file_io,
                    bounds,
                    Some(main.is_some_and(|head| head.snapshot_id() == snapshot.snapshot_id())),
                )
                .await?;
        }
        // A publication that never landed still owns the manifests its
        // persisted intent names; they live under the metadata directory and
        // nothing in the current snapshot references them.
        for manifest_list in unresolved_manifest_lists {
            reachable
                .scan_manifest_list(
                    manifest_list,
                    metadata.format_version(),
                    file_io,
                    bounds,
                    None,
                )
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
        live: Option<bool>,
    ) -> Result<(), OrphanError> {
        if !self.mark_scanned(manifest_list)? {
            return Ok(());
        }
        self.lists = self.lists.saturating_add(1);
        if self.lists > bounds.lists {
            return Err(OrphanError::ScanListsTooLarge {
                limit: bounds.lists,
            });
        }
        if live.is_some() {
            self.metadata(manifest_list)?;
        }
        let bytes = self.read(file_io, manifest_list, bounds).await?;
        let list = self
            .decode(move || ManifestList::parse_with_version(&bytes, format_version))
            .await?;
        for manifest in list.entries() {
            if live.is_some() {
                self.metadata(&manifest.manifest_path)?;
            }
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
            let bytes = self.read(file_io, &manifest.manifest_path, bounds).await?;
            let loaded = self.decode(move || Manifest::parse_avro(&bytes)).await?;
            for entry in loaded.entries() {
                self.entries = self.entries.saturating_add(1);
                if self.entries > bounds.max_entries() {
                    return Err(OrphanError::ScanTooLarge {
                        manifests: self.manifests,
                        entries: self.entries,
                    });
                }
                let file = entry.data_file();
                self.insert(file.file_path())?;
                if entry.is_alive()
                    && let Some(live) = live
                    && let Some(accounting) = &mut self.accounting
                {
                    let identity = ObjectIdentity::of(
                        &Location::parse(file.file_path()).map_err(OrphanError::Location)?,
                    );
                    let size = file.file_size_in_bytes();
                    let previous = accounting.data.entry(identity).or_insert((size, live));
                    if previous.0 != size {
                        return Err(OrphanError::InconsistentMetadata);
                    }
                    previous.1 |= live;
                }
                if self.entries.is_multiple_of(4096) {
                    tokio::task::yield_now().await;
                }
            }
        }
        Ok(())
    }

    async fn decode<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> iceberg::Result<T> + Send + 'static,
    ) -> Result<T, OrphanError> {
        let permit = self.decode_permit().await?;
        Ok(tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await??)
    }

    async fn decode_permit(
        &self,
    ) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, OrphanError> {
        // Cleanup has no reporting semaphore. Reporting keeps ownership in the
        // blocking closure, even if the waiting future is cancelled.
        match &self.decoder {
            Some(gate) => Ok(Some(
                gate.clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| OrphanError::InvalidPolicy)?,
            )),
            None => Ok(None),
        }
    }

    async fn read(
        &mut self,
        io: &FileIO,
        path: &str,
        bounds: ReachabilityBounds,
    ) -> Result<bytes::Bytes, OrphanError> {
        let remaining = bounds.total_bytes.saturating_sub(self.bytes);
        let limited = ReachabilityBounds {
            bytes: bounds.bytes.min(remaining),
            ..bounds
        };
        let bytes = read_bounded(io, path, limited)
            .await
            .map_err(|error| match error {
                OrphanError::ScanObjectTooLarge { size, .. }
                    if size <= bounds.bytes && size > remaining =>
                {
                    OrphanError::ScanBytesTooLarge {
                        limit: bounds.total_bytes,
                    }
                }
                other => other,
            })?;
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        Ok(bytes)
    }

    fn metadata(&mut self, path: &str) -> Result<(), OrphanError> {
        self.insert(path)?;
        if let Some(accounting) = &mut self.accounting {
            accounting.metadata.insert(ObjectIdentity::of(
                &Location::parse(path).map_err(OrphanError::Location)?,
            ));
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
    #[tokio::test]
    async fn a_cancelled_reporting_decode_keeps_its_budget_until_the_worker_exits() {
        let gate = Arc::new(tokio::sync::Semaphore::new(1));
        let scan = Reachable {
            decoder: Some(gate.clone()),
            ..Reachable::default()
        };
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let work = scan.decode(move || {
            started_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
            Ok(7)
        });
        let mut work = Box::pin(work);
        // Poll production decoding until the blocking worker has taken ownership.
        tokio::select! {
            () = async { started_rx.await.unwrap(); } => {},
            result = &mut work => panic!("decode finished before release: {result:?}"),
        }
        assert_eq!(gate.available_permits(), 0);
        // Cancelling the awaiting scan must not return the decoder's capacity.
        drop(work);
        assert_eq!(gate.available_permits(), 0);
        finish_tx.send(()).unwrap();
        let permit = tokio::time::timeout(Duration::from_secs(10), gate.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        assert_eq!(scan.decode(|| Ok(9)).await.unwrap(), 9);
    }
}
