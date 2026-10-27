//! Orphan-file selection (BS-031.T2, ADR 0062).
//!
//! This module decides which storage objects no longer matter. It reads the
//! table's manifests to prove what is still referenced and compares that set
//! with a listing of the table's storage. It performs no deletion: the caller
//! stores the immutable plan and the coordinator removes exactly that plan.
//!
//! Protection is conservative in five directions. The metadata file, the
//! metadata log, every manifest list, manifest, data file, delete file,
//! statistics file, and partition statistics file named by the current
//! metadata survive. Nothing under `<location>/bergsmith/` is ever a
//! candidate, because that tree holds worker outputs and artifacts a
//! publication may still add. An object younger than the effective window
//! survives. An object whose modification time is unknown survives. And a
//! reachability scan or listing that exceeds its bound fails the plan instead
//! of deleting from an incomplete view.

use std::collections::BTreeSet;

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{JobId, PlanId};
use bergsmith_storage::{Backend, Location, ObjectEntry};
use chrono::{DateTime, Utc};
use iceberg::io::FileIO;
use iceberg::spec::{Manifest, ManifestList, TableMetadata};
use serde::{Deserialize, Serialize};

use crate::baseline::DATA_PATH_PROPERTY;
use crate::error::PlanError;

/// The table state an orphan plan was built from.
///
/// Cleanup does not need a `main` snapshot: an empty table can still hold
/// abandoned files. The baseline therefore records the table identity, the
/// metadata file, and both locations, all of which exist without a snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrphanBaseline {
    /// The Iceberg `table-uuid`.
    pub table_uuid: uuid::Uuid,
    /// Where the current metadata was read from.
    pub metadata_location: String,
    /// The table's `location`.
    pub location: String,
    /// `write.data.path`, or `<location>/data`.
    pub data_location: String,
}

impl OrphanBaseline {
    /// Captures the table locations and the metadata file.
    ///
    /// # Errors
    ///
    /// [`PlanError::Location`] when the table or data location is not a valid
    /// storage location, and [`PlanError::StagedTable`] when the catalog
    /// returned no metadata file.
    pub fn capture(
        metadata: &TableMetadata,
        metadata_location: Option<&str>,
    ) -> Result<Self, PlanError> {
        let metadata_location = metadata_location.ok_or(PlanError::StagedTable {
            table_uuid: metadata.uuid(),
        })?;
        let location = Location::parse(metadata.location())?;
        let data_location = match metadata.properties().get(DATA_PATH_PROPERTY) {
            Some(path) => Location::parse(path)?,
            None => location.child("data")?,
        };
        Ok(Self {
            table_uuid: metadata.uuid(),
            metadata_location: metadata_location.to_owned(),
            location: location.to_string(),
            data_location: data_location.to_string(),
        })
    }
}

/// The hash domain of an orphan plan document.
const ORPHAN_PLAN_DOMAIN: &[u8] = b"bergsmith.maintenance.v1.OrphanPlan";

/// The orphan plan document layout version.
pub const ORPHAN_PLAN_VERSION: u32 = 1;

/// The path segment Bergsmith owns inside a table location.
const BERGSMITH_SEGMENT: &str = "bergsmith";

/// An invalid orphan policy.
#[derive(Debug, thiserror::Error)]
pub enum OrphanError {
    /// A bound is zero, so no plan could ever be built.
    #[error("the orphan clean-up bounds must be positive")]
    InvalidPolicy,
    /// A path named by table metadata is not a valid storage location.
    #[error("table metadata names an invalid location")]
    Location(#[source] bergsmith_storage::LocationError),
    /// Reading the metadata, manifest list, or manifest failed.
    #[error("reading table metadata failed")]
    Io(#[from] iceberg::Error),
    /// A storage listing or delete failed.
    #[error(transparent)]
    Storage(#[from] bergsmith_storage::StorageError),
    /// The reachability scan exceeds its bound, so the view is incomplete.
    #[error(
        "the reachability scan exceeded its bound with {manifests} manifests and {entries} entries"
    )]
    ScanTooLarge {
        /// Manifests read before the bound was crossed.
        manifests: usize,
        /// Manifest entries read before the bound was crossed.
        entries: usize,
    },
    /// The listing crossed the caller's bound.
    #[error("the table lists more objects than orphan cleanup may examine")]
    ListingTooLarge {
        /// The bound the caller set.
        limit: usize,
    },
    /// A manifest declares no entry counts, so its size cannot be bounded.
    #[error("manifest {path} declares no entry counts")]
    ScanUnboundedManifest {
        /// The manifest path.
        path: String,
    },
    /// A blocking decode task could not complete.
    #[error("decoding a manifest failed on a blocking task")]
    Blocking(#[from] tokio::task::JoinError),
    /// One manifest list or manifest is larger than the scan may read.
    #[error("a manifest file is larger than the scan bound")]
    ScanObjectTooLarge {
        /// The object's size in bytes.
        size: u64,
        /// The bound the caller set.
        limit: u64,
    },
}

/// The bounds of one reachability scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReachabilityBounds {
    /// The most manifests one scan reads.
    pub max_manifests: usize,
    /// The most manifest entries one scan reads.
    pub max_entries: usize,
    /// The largest manifest list or manifest one scan reads, in bytes.
    pub max_bytes: u64,
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
            max_manifests,
            max_entries,
            max_bytes,
        })
    }
}

/// The identity of one stored object, independent of its URI spelling.
///
/// `s3` and `s3a` name the same object, and so do other scheme aliases for one
/// backend. Reachability stores identities, so a live file written as `s3a`
/// cannot be missed because the listing spells it `s3`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ObjectIdentity(Backend, String, String);

impl ObjectIdentity {
    fn of(location: &Location) -> Self {
        Self(
            location.backend(),
            location.authority().to_owned(),
            location.key().to_owned(),
        )
    }
}

/// Every object the table still references.
///
/// The set is built from the current metadata only. Metadata files named by
/// `metadata-log` are protected, but the files those older versions reference
/// are not: this release cannot parse an older metadata file, so it matches
/// Iceberg's own orphan-file action, which also protects only what the current
/// metadata retains.
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
    /// [`OrphanError::Io`] when reading a manifest list or manifest fails, and
    /// [`OrphanError::ScanTooLarge`] when `bounds` is exceeded.
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
        if size > bounds.max_bytes {
            return Err(OrphanError::ScanObjectTooLarge {
                size,
                limit: bounds.max_bytes,
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
            if self.manifests > bounds.max_manifests {
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
                > bounds.max_entries
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
            if size > bounds.max_bytes {
                return Err(OrphanError::ScanObjectTooLarge {
                    size,
                    limit: bounds.max_bytes,
                });
            }
            let bytes = file_io.new_input(&manifest.manifest_path)?.read().await?;
            let loaded =
                tokio::task::spawn_blocking(move || Manifest::parse_avro(&bytes)).await??;
            for entry in loaded.entries() {
                self.entries = self.entries.saturating_add(1);
                if self.entries > bounds.max_entries {
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

    fn insert(&mut self, path: &str) -> Result<(), OrphanError> {
        let location = Location::parse(path).map_err(OrphanError::Location)?;
        self.files.insert(ObjectIdentity::of(&location));
        Ok(())
    }
}

/// The age and size rules applied to one table's listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrphanPolicy {
    window_ms: i64,
    max_candidates: usize,
}

impl OrphanPolicy {
    /// Builds a policy from the operation's minimum age, the connection's
    /// protection window, and the candidate bound.
    ///
    /// The effective window is the larger of the two ages, so a connection
    /// that promises a longer execution and publication protection keeps
    /// objects for that long even when the operation's minimum is shorter.
    ///
    /// # Errors
    ///
    /// [`OrphanError::InvalidPolicy`] when `max_candidates` is zero.
    pub fn new(
        minimum_age_seconds: u64,
        protection_ms: i64,
        max_candidates: usize,
    ) -> Result<Self, OrphanError> {
        if max_candidates == 0 {
            return Err(OrphanError::InvalidPolicy);
        }
        let minimum_ms = i64::try_from(minimum_age_seconds)
            .unwrap_or(i64::MAX / 1_000)
            .saturating_mul(1_000);
        let window_ms = minimum_ms.max(protection_ms.max(0));
        Ok(Self {
            window_ms,
            max_candidates,
        })
    }

    /// The effective age window in milliseconds.
    #[must_use]
    pub const fn window_ms(&self) -> i64 {
        self.window_ms
    }

    /// The most candidates one plan may carry.
    #[must_use]
    pub const fn max_candidates(&self) -> usize {
        self.max_candidates
    }

    /// The newest modification time a candidate may have.
    #[must_use]
    pub fn cutoff_ms(&self, now: DateTime<Utc>) -> i64 {
        now.timestamp_millis().saturating_sub(self.window_ms)
    }
}

/// One object the plan may delete.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrphanCandidate {
    /// The object's full location.
    pub path: String,
    /// The listed size in bytes.
    pub size_bytes: u64,
    /// The listed modification time in milliseconds since the Unix epoch.
    pub last_modified_ms: i64,
}

/// The outcome of one selection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OrphanDecision {
    /// The effective timestamp cutoff in milliseconds since the Unix epoch.
    pub cutoff_ms: i64,
    /// Objects to delete, oldest first, then by path.
    pub candidates: Vec<OrphanCandidate>,
    /// Candidates beyond the plan's bound that wait for a later job.
    pub deferred: u64,
    /// Listed objects kept because they are too young.
    pub retained_young: u64,
    /// Listed objects kept because the table references them.
    pub retained_referenced: u64,
    /// Listed objects kept because Bergsmith owns their prefix.
    pub retained_bergsmith: u64,
}

impl OrphanDecision {
    /// Whether nothing is deletable.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }
}

/// Whether `location` lies under a `bergsmith` path segment of the table.
///
/// A table's data location can change (`write.data.path`), and outputs written
/// under the previous one are still Bergsmith's own. Any `bergsmith` segment
/// below the table location therefore marks the object as protected, which
/// also covers layouts this release does not know about. Over-retention of a
/// user directory that happens to be named `bergsmith` is the accepted cost.
fn is_berggsmith_owned(location: &Location, table: &Location) -> bool {
    if !location.is_within(table) {
        return false;
    }
    let key = location.key();
    let relative = key
        .strip_prefix(table.key())
        .map_or(key, |rest| rest.trim_start_matches('/'));
    relative
        .split('/')
        .any(|segment| segment == BERGSMITH_SEGMENT)
}

/// The Bergsmith-owned trees inside and beside a table location.
/// The prefix checks a deletion pass applies before removing an object.
///
/// # Errors
///
/// [`OrphanError::Location`] when either location is invalid.
pub fn protected_prefixes(
    table_location: &str,
    data_location: &str,
) -> Result<Vec<Location>, OrphanError> {
    let table = Location::parse(table_location).map_err(OrphanError::Location)?;
    bergsmith_roots(&table, data_location)
}

fn bergsmith_roots(table: &Location, data_location: &str) -> Result<Vec<Location>, OrphanError> {
    let data = Location::parse(data_location).map_err(OrphanError::Location)?;
    let mut roots = vec![
        table
            .child(BERGSMITH_SEGMENT)
            .map_err(OrphanError::Location)?,
        data.child(BERGSMITH_SEGMENT)
            .map_err(OrphanError::Location)?,
    ];
    roots.sort_unstable();
    roots.dedup();
    Ok(roots)
}

/// What one deletion check decided for a planned object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateVerdict {
    /// The object is still unreferenced, old, and unchanged.
    Delete,
    /// The object became referenced, young, replaced, or protected.
    Skip,
    /// The object was already gone when the batch looked.
    Absent,
}

impl CandidateVerdict {
    /// Whether the verdict resolves the candidate without deleting it.
    #[must_use]
    pub const fn is_resolved_without_deletion(self) -> bool {
        matches!(self, Self::Skip | Self::Absent)
    }
}

/// Re-checks one planned candidate against the current table and listing.
///
/// Planning and deletion share this predicate, so a batch applies exactly the
/// rules the immutable plan was selected with. `fresh` is the object as the
/// current listing sees it: a missing entry means the object is gone, a
/// different size means it is not the object the plan named, and an unknown
/// modification time cannot prove age.
#[must_use]
pub fn verify_candidate(
    candidate: &OrphanCandidate,
    location: &Location,
    fresh: Option<&(u64, Option<i64>)>,
    reachable: &Reachable,
    prefixes: &[Location],
    table: &Location,
    cutoff_ms: i64,
) -> CandidateVerdict {
    if !location.is_within(table)
        || prefixes.iter().any(|prefix| location.is_within(prefix))
        || is_berggsmith_owned(location, table)
        || reachable.contains(location)
    {
        return CandidateVerdict::Skip;
    }
    match fresh {
        None => CandidateVerdict::Absent,
        Some((size, Some(modified))) if *size == candidate.size_bytes && *modified <= cutoff_ms => {
            CandidateVerdict::Delete
        }
        Some(_) => CandidateVerdict::Skip,
    }
}

/// Selects deletable objects from a listing.
///
/// `listed` must come from [`ObjectStore::list`] over the table location, so
/// every entry is inside it. Entries that do not belong to the table are
/// ignored, which keeps the function safe if a caller passes a wider listing.
///
/// Bergsmith writes its attempt outputs and artifacts under
/// `<data-location>/bergsmith/`, where the data location is the table's
/// `write.data.path` or `<table-location>/data`. Both that tree and a
/// `<table-location>/bergsmith/` tree are protected, so neither a custom data
/// path nor an older layout can lose outputs to cleanup.
///
/// [`ObjectStore::list`]: bergsmith_storage::ObjectStore::list
///
/// # Errors
///
/// [`OrphanError::Location`] when a location argument is not valid.
pub fn select_orphans(
    reachable: &Reachable,
    listed: &[ObjectEntry],
    table_location: &str,
    data_location: &str,
    policy: &OrphanPolicy,
    now: DateTime<Utc>,
) -> Result<OrphanDecision, OrphanError> {
    let table = Location::parse(table_location).map_err(OrphanError::Location)?;
    let bergsmith_roots = bergsmith_roots(&table, data_location)?;
    let cutoff_ms = policy.cutoff_ms(now);
    let mut decision = OrphanDecision {
        cutoff_ms,
        ..OrphanDecision::default()
    };
    let mut candidates = Vec::new();
    for entry in listed {
        if !entry.location.is_within(&table) {
            continue;
        }
        let Some(last_modified_ms) = entry.last_modified_ms else {
            // An unknown time cannot prove age, so the object survives.
            decision.retained_young = decision.retained_young.saturating_add(1);
            continue;
        };
        let candidate = OrphanCandidate {
            path: entry.location.to_string(),
            size_bytes: entry.size,
            last_modified_ms,
        };
        let verdict = verify_candidate(
            &candidate,
            &entry.location,
            Some(&(entry.size, entry.last_modified_ms)),
            reachable,
            &bergsmith_roots,
            &table,
            cutoff_ms,
        );
        match verdict {
            CandidateVerdict::Delete => candidates.push(candidate),
            CandidateVerdict::Skip => {
                if is_berggsmith_owned(&entry.location, &table)
                    || bergsmith_roots
                        .iter()
                        .any(|root| entry.location.is_within(root))
                {
                    decision.retained_bergsmith = decision.retained_bergsmith.saturating_add(1);
                } else if reachable.contains(&entry.location) {
                    decision.retained_referenced = decision.retained_referenced.saturating_add(1);
                } else {
                    decision.retained_young = decision.retained_young.saturating_add(1);
                }
            }
            // A listed entry always exists, so this arm is unreachable.
            CandidateVerdict::Absent => {}
        }
    }
    // The plan keeps the oldest objects first, so a bounded job reclaims the
    // most valuable space and later runs continue where this one stopped.
    candidates.sort_unstable_by(|left, right| {
        (left.last_modified_ms, &left.path).cmp(&(right.last_modified_ms, &right.path))
    });
    if candidates.len() > policy.max_candidates {
        decision.deferred =
            u64::try_from(candidates.len() - policy.max_candidates).unwrap_or(u64::MAX);
        candidates.truncate(policy.max_candidates);
    }
    decision.candidates = candidates;
    Ok(decision)
}

/// An immutable, hashed orphan-cleanup plan (ADR 0062).
///
/// The document records the baseline the selection was made from, the
/// effective cutoff, what the scan saw, and the exact objects to delete or
/// skip. It is the only input to deletion, so a rerun of the planner or a
/// later listing cannot change what a job removes.
///
/// The canonical JSON is the serialized typed form: field order is fixed and
/// the candidate list is sorted. The hash is
/// `SHA-256("bergsmith.maintenance.v1.OrphanPlan" || 0x00 || JSON)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrphanPlanDocument {
    /// The document layout version; always [`ORPHAN_PLAN_VERSION`].
    pub version: u32,
    /// The job, which must be in `Planning` when the document is stored.
    pub job_id: JobId,
    /// The plan's identifier.
    pub plan_id: PlanId,
    /// The table state the selection was made from.
    pub baseline: OrphanBaseline,
    /// The table location the listing covered.
    pub table_location: String,
    /// The data location whose `bergsmith/` tree is protected.
    pub data_location: String,
    /// The effective timestamp cutoff, in milliseconds since the Unix epoch.
    pub cutoff_ms: i64,
    /// The number of listed objects the selection considered.
    pub listed_objects: u64,
    /// The number of referenced objects the reachability scan found.
    pub referenced_objects: u64,
    /// Objects to delete, oldest first, then by path.
    pub candidates: Vec<OrphanCandidate>,
    /// Candidates beyond the bound that wait for a later job.
    pub deferred: u64,
}

impl OrphanPlanDocument {
    /// Assembles a document from a selection and the scan that produced it.
    ///
    /// # Errors
    ///
    /// [`PlanError::InvalidConfig`] when the decision's candidates are not
    /// strictly ordered and unique, which would make the plan ambiguous.
    pub fn new(
        job_id: JobId,
        plan_id: PlanId,
        baseline: OrphanBaseline,
        table_location: &str,
        listed_objects: u64,
        reachable: &Reachable,
        decision: &OrphanDecision,
    ) -> Result<Self, PlanError> {
        let baseline_data_location = baseline.data_location.clone();
        if decision.candidates.windows(2).any(|pair| {
            (pair[0].last_modified_ms, &pair[0].path) >= (pair[1].last_modified_ms, &pair[1].path)
        }) {
            return Err(PlanError::InvalidConfig(
                "orphan candidates must be ordered and unique".to_owned(),
            ));
        }
        Ok(Self {
            version: ORPHAN_PLAN_VERSION,
            job_id,
            plan_id,
            baseline,
            table_location: table_location.to_owned(),
            data_location: baseline_data_location.clone(),
            cutoff_ms: decision.cutoff_ms,
            listed_objects,
            referenced_objects: u64::try_from(reachable.len()).unwrap_or(u64::MAX),
            candidates: decision.candidates.clone(),
            deferred: decision.deferred,
        })
    }

    /// The canonical JSON of the document.
    ///
    /// # Errors
    ///
    /// [`PlanError::CorruptPlan`] if serialization fails, which a document
    /// built by [`new`](Self::new) never does.
    pub fn to_json(&self) -> Result<Vec<u8>, PlanError> {
        serde_json::to_vec(self).map_err(|error| {
            PlanError::CorruptPlan(format!("the orphan plan does not encode: {error}"))
        })
    }

    /// The document as a JSON value, for storage.
    ///
    /// # Errors
    ///
    /// As for [`to_json`](Self::to_json).
    pub fn to_value(&self) -> Result<serde_json::Value, PlanError> {
        serde_json::to_value(self).map_err(|error| {
            PlanError::CorruptPlan(format!("the orphan plan does not encode: {error}"))
        })
    }

    /// The document hash.
    ///
    /// # Errors
    ///
    /// As for [`to_json`](Self::to_json).
    pub fn content_hash(&self) -> Result<Sha256Digest, PlanError> {
        Ok(Sha256Digest::of(ORPHAN_PLAN_DOMAIN, &self.to_json()?))
    }

    /// Reads a persisted document.
    ///
    /// Only the layout version is checked here. Persistence compares the
    /// recomputed [`content_hash`](Self::content_hash) with the stored hash
    /// separately, so a document that decodes but does not match its hash is
    /// rejected there.
    ///
    /// # Errors
    ///
    /// [`PlanError::CorruptPlan`] when the content does not decode or its
    /// version is not [`ORPHAN_PLAN_VERSION`].
    pub fn from_persisted(value: serde_json::Value) -> Result<Self, PlanError> {
        let document: Self = serde_json::from_value(value).map_err(|error| {
            PlanError::CorruptPlan(format!("the orphan plan does not decode: {error}"))
        })?;
        if document.version != ORPHAN_PLAN_VERSION {
            return Err(PlanError::CorruptPlan(format!(
                "orphan plan version {} is not supported",
                document.version
            )));
        }
        Ok(document)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(path: &str) -> Location {
        Location::parse(path).unwrap()
    }

    fn entry(path: &str, age_ms: i64, now: DateTime<Utc>) -> ObjectEntry {
        ObjectEntry {
            location: location(path),
            size: 10,
            last_modified_ms: Some(now.timestamp_millis() - age_ms),
        }
    }

    fn policy() -> OrphanPolicy {
        OrphanPolicy::new(3 * 24 * 60 * 60, 0, 100).unwrap()
    }

    fn baseline() -> OrphanBaseline {
        OrphanBaseline {
            table_uuid: uuid::Uuid::nil(),
            metadata_location: "s3://b/t/metadata/v1.metadata.json".to_owned(),
            location: "s3://b/t".to_owned(),
            data_location: "s3://b/t/data".to_owned(),
        }
    }

    #[test]
    fn the_effective_window_is_the_larger_age() {
        let shortest = OrphanPolicy::new(60, 5_000, 10).unwrap();
        assert_eq!(shortest.window_ms(), 60_000);
        let connection = OrphanPolicy::new(60, 120_000, 10).unwrap();
        assert_eq!(connection.window_ms(), 120_000);
        assert!(matches!(
            OrphanPolicy::new(1, 0, 0),
            Err(OrphanError::InvalidPolicy)
        ));
    }

    #[test]
    fn only_old_unreferenced_objects_outside_bergsmith_are_candidates() {
        let now = Utc::now();
        let mut reachable = Reachable::default();
        reachable.insert("s3://b/t/metadata/v2.json").unwrap();
        reachable.insert("s3://b/t/data/live.parquet").unwrap();
        let listed = vec![
            entry("s3://b/t/data/live.parquet", 90 * 86_400_000, now),
            entry("s3://b/t/data/orphan.parquet", 90 * 86_400_000, now),
            entry("s3://b/t/data/young.parquet", 1_000, now),
            entry(
                "s3://b/t/bergsmith/job/attempt/out.parquet",
                90 * 86_400_000,
                now,
            ),
            entry("s3://b/t/metadata/v2.json", 90 * 86_400_000, now),
        ];
        let decision = select_orphans(
            &reachable,
            &listed,
            "s3://b/t",
            "s3://b/t/data",
            &policy(),
            now,
        )
        .unwrap();
        assert_eq!(decision.candidates.len(), 1);
        assert_eq!(decision.candidates[0].path, "s3://b/t/data/orphan.parquet");
        assert_eq!(decision.retained_referenced, 2);
        assert_eq!(decision.retained_bergsmith, 1);
        assert_eq!(decision.retained_young, 1);
    }

    #[test]
    fn an_unknown_modification_time_is_never_a_candidate() {
        let now = Utc::now();
        let listed = vec![ObjectEntry {
            location: location("s3://b/t/data/unknown.parquet"),
            size: 1,
            last_modified_ms: None,
        }];
        let decision = select_orphans(
            &Reachable::default(),
            &listed,
            "s3://b/t",
            "s3://b/t/data",
            &policy(),
            now,
        )
        .unwrap();
        assert!(decision.candidates.is_empty());
        assert_eq!(decision.retained_young, 1);
    }

    #[test]
    fn outside_the_table_location_is_ignored() {
        let now = Utc::now();
        let listed = vec![
            entry("s3://other/t/data/x.parquet", 90 * 86_400_000, now),
            entry("s3://b/t-other/data/x.parquet", 90 * 86_400_000, now),
        ];
        let decision = select_orphans(
            &Reachable::default(),
            &listed,
            "s3://b/t",
            "s3://b/t/data",
            &policy(),
            now,
        )
        .unwrap();
        assert!(decision.candidates.is_empty());
        assert_eq!(decision.retained_young, 0);
    }

    #[test]
    fn the_candidate_bound_keeps_the_oldest_and_defers_the_rest() {
        let now = Utc::now();
        let listed: Vec<_> = (0..5)
            .map(|index| ObjectEntry {
                location: location(&format!("s3://b/t/data/{index}.parquet")),
                size: 1,
                last_modified_ms: Some(now.timestamp_millis() - 10 * 86_400_000 + index),
            })
            .collect();
        let bounded = OrphanPolicy::new(86_400, 0, 2).unwrap();
        let decision = select_orphans(
            &Reachable::default(),
            &listed,
            "s3://b/t",
            "s3://b/t/data",
            &bounded,
            now,
        )
        .unwrap();
        assert_eq!(decision.candidates.len(), 2);
        assert_eq!(decision.deferred, 3);
        assert_eq!(
            decision.candidates[0].last_modified_ms,
            now.timestamp_millis() - 10 * 86_400_000
        );
    }

    #[test]
    fn deletion_verdicts_follow_the_plan_rules() {
        let now = Utc::now();
        let table = location("s3://b/t");
        let prefixes = protected_prefixes("s3://b/t", "s3://b/t/data").unwrap();
        let candidate = OrphanCandidate {
            path: "s3://b/t/data/old.parquet".to_owned(),
            size_bytes: 10,
            last_modified_ms: now.timestamp_millis() - 90 * 86_400_000,
        };
        let location = location(&candidate.path);
        let stale = now.timestamp_millis() - 86_400_000;
        let fresh = (10, Some(candidate.last_modified_ms));

        assert_eq!(
            verify_candidate(
                &candidate,
                &location,
                Some(&fresh),
                &Reachable::default(),
                &prefixes,
                &table,
                stale,
            ),
            CandidateVerdict::Delete
        );
        assert_eq!(
            verify_candidate(
                &candidate,
                &location,
                Some(&fresh),
                &Reachable::default(),
                &prefixes,
                &table,
                candidate.last_modified_ms - 1,
            ),
            CandidateVerdict::Skip,
            "an object inside the window is kept"
        );
        assert_eq!(
            verify_candidate(
                &candidate,
                &location,
                Some(&(11, Some(candidate.last_modified_ms))),
                &Reachable::default(),
                &prefixes,
                &table,
                stale,
            ),
            CandidateVerdict::Skip,
            "a different size is not the object the plan named"
        );
        let mut reachable = Reachable::default();
        reachable.insert(&candidate.path).unwrap();
        assert_eq!(
            verify_candidate(
                &candidate,
                &location,
                Some(&fresh),
                &reachable,
                &prefixes,
                &table,
                stale
            ),
            CandidateVerdict::Skip,
            "a newly referenced object is kept"
        );
        assert_eq!(
            verify_candidate(
                &candidate,
                &location,
                None,
                &Reachable::default(),
                &prefixes,
                &table,
                stale,
            ),
            CandidateVerdict::Absent
        );
        assert_eq!(
            verify_candidate(
                &candidate,
                &location,
                Some(&fresh),
                &Reachable::default(),
                &prefixes,
                &table,
                stale,
            ),
            CandidateVerdict::Delete
        );
        assert!(CandidateVerdict::Absent.is_resolved_without_deletion());
    }

    #[test]
    fn scheme_aliases_of_a_live_file_are_still_reachable() {
        let now = Utc::now();
        let mut reachable = Reachable::default();
        reachable.insert("s3a://b/t/data/live.parquet").unwrap();
        let listed = vec![
            entry("s3://b/t/data/live.parquet", 90 * 86_400_000, now),
            entry("s3a://b/t/data/other.parquet", 90 * 86_400_000, now),
        ];
        let decision = select_orphans(
            &reachable,
            &listed,
            "s3://b/t",
            "s3://b/t/data",
            &policy(),
            now,
        )
        .unwrap();
        assert_eq!(decision.retained_referenced, 1);
        assert_eq!(decision.candidates[0].path, "s3a://b/t/data/other.parquet");
    }

    #[test]
    fn outputs_under_a_previous_data_location_are_protected() {
        let now = Utc::now();
        let listed = vec![
            // The table once used `old-data`; its Bergsmith tree must survive.
            entry(
                "s3://b/t/old-data/bergsmith/job/attempt/out.parquet",
                90 * 86_400_000,
                now,
            ),
            entry("s3://b/t/old-data/orphan.parquet", 90 * 86_400_000, now),
        ];
        let decision = select_orphans(
            &Reachable::default(),
            &listed,
            "s3://b/t",
            "s3://b/t/new-data",
            &policy(),
            now,
        )
        .unwrap();
        assert_eq!(decision.retained_bergsmith, 1);
        assert_eq!(decision.candidates.len(), 1);
        assert_eq!(
            decision.candidates[0].path,
            "s3://b/t/old-data/orphan.parquet"
        );
    }

    #[test]
    fn the_oldest_candidates_survive_the_bound_even_when_paths_sort_elsewhere() {
        let now = Utc::now();
        let listed = vec![
            ObjectEntry {
                location: location("s3://b/t/data/a.parquet"),
                size: 1,
                last_modified_ms: Some(now.timestamp_millis() - 10 * 86_400_000),
            },
            ObjectEntry {
                location: location("s3://b/t/data/z.parquet"),
                size: 1,
                last_modified_ms: Some(now.timestamp_millis() - 90 * 86_400_000),
            },
        ];
        let bounded = OrphanPolicy::new(86_400, 0, 1).unwrap();
        let decision = select_orphans(
            &Reachable::default(),
            &listed,
            "s3://b/t",
            "s3://b/t/data",
            &bounded,
            now,
        )
        .unwrap();
        assert_eq!(
            decision.candidates[0].path, "s3://b/t/data/z.parquet",
            "the bound keeps the oldest object, not the first path"
        );
    }

    #[test]
    fn a_custom_data_location_bergsmith_tree_is_protected() {
        let now = Utc::now();
        let listed = vec![
            entry(
                "s3://b/t/custom-data/bergsmith/job/attempt/out.parquet",
                90 * 86_400_000,
                now,
            ),
            entry("s3://b/t/custom-data/orphan.parquet", 90 * 86_400_000, now),
        ];
        let decision = select_orphans(
            &Reachable::default(),
            &listed,
            "s3://b/t",
            "s3://b/t/custom-data",
            &policy(),
            now,
        )
        .unwrap();
        assert_eq!(decision.retained_bergsmith, 1);
        assert_eq!(decision.candidates.len(), 1);
        assert_eq!(
            decision.candidates[0].path, "s3://b/t/custom-data/orphan.parquet",
            "outputs under a custom data path are never candidates"
        );
    }

    #[test]
    fn selection_is_deterministic_for_the_same_listing() {
        let now = Utc::now();
        let listed = vec![
            entry("s3://b/t/data/b.parquet", 10 * 86_400_000, now),
            entry("s3://b/t/data/a.parquet", 10 * 86_400_000, now),
        ];
        let first = select_orphans(
            &Reachable::default(),
            &listed,
            "s3://b/t",
            "s3://b/t/data",
            &policy(),
            now,
        )
        .unwrap();
        let second = select_orphans(
            &Reachable::default(),
            &listed,
            "s3://b/t",
            "s3://b/t/data",
            &policy(),
            now,
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.candidates[0].path, "s3://b/t/data/a.parquet");
    }

    #[test]
    fn the_plan_document_is_stable_and_hash_bound() {
        let now = Utc::now();
        let listed = vec![entry("s3://b/t/data/a.parquet", 90 * 86_400_000, now)];
        let decision = select_orphans(
            &Reachable::default(),
            &listed,
            "s3://b/t",
            "s3://b/t/data",
            &policy(),
            now,
        )
        .unwrap();
        let baseline = baseline();
        let job_id = JobId::generate();
        let plan_id = PlanId::generate();
        let document = OrphanPlanDocument::new(
            job_id,
            plan_id,
            baseline.clone(),
            "s3://b/t",
            1,
            &Reachable::default(),
            &decision,
        )
        .unwrap();
        let hash = document.content_hash().unwrap();
        let round_trip = OrphanPlanDocument::from_persisted(document.to_value().unwrap()).unwrap();
        assert_eq!(round_trip, document);
        assert_eq!(round_trip.content_hash().unwrap().to_hex(), hash.to_hex());
        assert_eq!(document.version, ORPHAN_PLAN_VERSION);
        assert_eq!(document.candidates.len(), 1);
    }

    #[test]
    fn an_unordered_candidate_list_is_rejected() {
        let decision = OrphanDecision {
            candidates: vec![
                OrphanCandidate {
                    path: "s3://b/t/data/b".to_owned(),
                    size_bytes: 1,
                    last_modified_ms: 1,
                },
                OrphanCandidate {
                    path: "s3://b/t/data/a".to_owned(),
                    size_bytes: 1,
                    last_modified_ms: 1,
                },
            ],
            ..OrphanDecision::default()
        };
        assert!(matches!(
            OrphanPlanDocument::new(
                JobId::generate(),
                PlanId::generate(),
                baseline(),
                "s3://b/t",
                2,
                &Reachable::default(),
                &decision,
            ),
            Err(PlanError::InvalidConfig(_))
        ));
    }
}
