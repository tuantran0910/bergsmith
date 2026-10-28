//! Physical table-prefix accounting, separate from deletion authority.

use crate::orphans::reachable::ObjectIdentity;
use crate::orphans::{
    self, CandidateCheck, CandidateVerdict, OrphanCandidate, OrphanError, ReachabilityBounds,
    Reachable,
};
use bergsmith_storage::{Location, ObjectEntry};
use chrono::{DateTime, Utc};
use iceberg::{io::FileIO, spec::TableMetadata};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};

/// Unique object counts and listed bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    /// Unique objects.
    pub files: u64,
    /// Listed bytes (manifest bytes for the separately reported external files).
    pub bytes: u64,
}
impl Usage {
    fn add(&mut self, bytes: u64) -> Result<(), OrphanError> {
        self.files = self
            .files
            .checked_add(1)
            .ok_or(OrphanError::AccountingOverflow)?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(OrphanError::AccountingOverflow)?;
        Ok(())
    }
}

/// A complete observation, not an atomic object-store snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageBreakdown {
    /// Live data and delete objects of main.
    pub live: Usage,
    /// Live objects of other retained snapshots, excluding main's objects.
    pub retained: Usage,
    /// Reachable metadata, manifests and statistics objects.
    pub metadata: Usage,
    /// Objects eligible under the cleanup predicate; cleanup must re-verify.
    pub orphan: Usage,
    /// Other protected, young, unknown-age, or conservatively reachable objects.
    pub protected: Usage,
    /// Total listed objects; equals the sum of the five buckets.
    pub total: Usage,
    /// Referenced live data/delete files outside the table prefix, excluded above.
    pub external: Usage,
    /// Effective inclusive modification-time cutoff, milliseconds since epoch.
    pub cutoff_ms: i64,
}

/// Reporting's scan bounds and decoder budget, isolated from cleanup.
#[derive(Clone, Debug)]
pub struct StorageScan {
    bounds: ReachabilityBounds,
    decoder: Arc<tokio::sync::Semaphore>,
}
impl StorageScan {
    /// Shares reporting's decoder budget across scans and detached blocking tasks.
    #[must_use]
    pub fn new(bounds: ReachabilityBounds, decoder: Arc<tokio::sync::Semaphore>) -> Self {
        Self { bounds, decoder }
    }

    /// Captures bounded reachability and classifies a complete table-root listing.
    /// Neither data nor delete contents are read. Candidate counts are uncapped.
    ///
    /// # Errors
    /// Returns typed scan, inconsistent listing/metadata, or accounting overflow
    /// errors. No partial result is returned. A closed decoder gate is invalid policy.
    pub async fn collect(
        &self,
        metadata: &TableMetadata,
        metadata_location: &str,
        unresolved: &[String],
        io: &FileIO,
        listed: &[ObjectEntry],
        cutoff: DateTime<Utc>,
    ) -> Result<StorageBreakdown, OrphanError> {
        let reachable = Reachable::capture_accounted(
            metadata,
            metadata_location,
            unresolved,
            io,
            self.bounds,
            self.decoder.clone(),
        )
        .await?;
        let table = Location::parse(metadata.location()).map_err(OrphanError::Location)?;
        classify(&reachable, listed, &table, cutoff.timestamp_millis()).await
    }
}

async fn classify(
    reachable: &Reachable,
    listed: &[ObjectEntry],
    table: &Location,
    cutoff_ms: i64,
) -> Result<StorageBreakdown, OrphanError> {
    let references = reachable
        .accounting
        .as_ref()
        .ok_or(OrphanError::InvalidPolicy)?;
    // Segment ownership also covers historical/custom data paths under root.
    let check = CandidateCheck {
        table,
        prefixes: &[],
        reachable,
        cutoff_ms,
    };
    let mut result = StorageBreakdown {
        cutoff_ms,
        ..StorageBreakdown::default()
    };
    let mut seen = BTreeMap::new();
    for (index, entry) in listed.iter().enumerate() {
        if !entry.location.is_within(table) {
            return Err(OrphanError::InconsistentListing);
        }
        let identity = ObjectIdentity::of(&entry.location);
        if let Some(previous) = seen.insert(identity.clone(), (entry.size, entry.last_modified_ms))
        {
            if previous != (entry.size, entry.last_modified_ms) {
                return Err(OrphanError::InconsistentListing);
            }
            continue;
        }
        result.total.add(entry.size)?;
        let bucket = if references.metadata.contains(&identity) {
            &mut result.metadata
        } else if let Some((_, live)) = references.data.get(&identity) {
            if *live {
                &mut result.live
            } else {
                &mut result.retained
            }
        } else {
            let candidate = OrphanCandidate {
                path: entry.location.to_string(),
                size_bytes: entry.size,
                last_modified_ms: entry.last_modified_ms.unwrap_or(i64::MAX),
            };
            if verify(&candidate, entry, &check) {
                &mut result.orphan
            } else {
                &mut result.protected
            }
        };
        bucket.add(entry.size)?;
        if index.is_multiple_of(4096) {
            tokio::task::yield_now().await;
        }
    }
    for (identity, (bytes, _)) in &references.data {
        // Identities are canonical; prefix membership still uses typed Locations.
        if !identity.is_within(table) {
            result.external.add(*bytes)?;
        }
    }
    Ok(result)
}

fn verify(candidate: &OrphanCandidate, entry: &ObjectEntry, check: &CandidateCheck<'_>) -> bool {
    orphans::verify_candidate(candidate, Some(entry), check) == CandidateVerdict::Delete
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orphans::reachable::References;
    use std::collections::BTreeSet;

    fn entry(path: &str, size: u64, modified: Option<i64>) -> ObjectEntry {
        ObjectEntry {
            location: Location::parse(path).unwrap(),
            size,
            last_modified_ms: modified,
        }
    }
    #[tokio::test]
    async fn categories_partition_the_listing_without_capping_candidates() {
        let listed = vec![
            entry("s3://b/t/live", 10, Some(1)),
            entry("s3a://b/t/live", 10, Some(1)),
            entry("s3://b/t/retained", 20, Some(1)),
            entry("s3://b/t/meta", 30, Some(1)),
            entry("s3://b/t/orphan1", 40, Some(1)),
            entry("s3://b/t/orphan2", 50, Some(1)),
            entry("s3://b/t/young", 60, Some(101)),
            entry("s3://b/t/unknown", 70, None),
            entry("s3://b/t/bergsmith/output", 80, Some(1)),
            entry("s3://b/t/intent", 90, Some(1)),
        ];
        let id = |p: &str| ObjectIdentity::of(&Location::parse(p).unwrap());
        let mut reachable = Reachable::default();
        for p in ["live", "retained", "meta", "intent"] {
            reachable.insert(&format!("s3://b/t/{p}")).unwrap();
        }
        reachable.accounting = Some(References {
            metadata: BTreeSet::from([id("s3://b/t/meta")]),
            data: BTreeMap::from([
                (id("s3://b/t/live"), (10, true)),
                (id("s3://b/t/retained"), (20, false)),
                (id("s3://outside/file"), (100, true)),
            ]),
        });
        let root = Location::parse("s3://b/t").unwrap();
        let result = classify(&reachable, &listed, &root, 100).await.unwrap();
        assert_eq!(
            result.total,
            Usage {
                files: 9,
                bytes: 450
            }
        );
        assert_eq!(result.live.bytes, 10);
        assert_eq!(result.retained.bytes, 20);
        assert_eq!(result.metadata.bytes, 30);
        assert_eq!(
            result.orphan,
            Usage {
                files: 2,
                bytes: 90
            }
        );
        assert_eq!(result.protected.bytes, 300);
        assert_eq!(result.external.bytes, 100);
        let mut conflicting = listed;
        conflicting.push(entry("s3://b/t/live", 11, Some(1)));
        assert!(
            classify(&reachable, &conflicting, &root, 100)
                .await
                .is_err()
        );
        assert!(
            classify(&reachable, &[entry("s3://b/other", 1, Some(1))], &root, 100)
                .await
                .is_err()
        );
    }
    #[test]
    fn accounting_overflow_is_not_a_plausible_zero() {
        let mut usage = Usage {
            files: 1,
            bytes: u64::MAX,
        };
        assert!(matches!(usage.add(1), Err(OrphanError::AccountingOverflow)));
    }
}
