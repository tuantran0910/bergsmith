//! Snapshot expiration selection (BS-031.T1, ADR 0061).
//!
//! This module decides which snapshot entries may leave the table metadata. It
//! performs no I/O and sends no commit: the caller stores the decision, takes
//! the table publication barrier, and publishes an exact `remove-snapshots`
//! update. Files are never deleted here; unreferenced files belong to the
//! separate cleanup tasks.
//!
//! Selection is conservative in three directions. Refs are never aged out, so
//! every branch and tag head survives. Snapshots pinned by the ledger or newer
//! than the effective cutoff survive. The effective cutoff and minimum are the
//! stricter of the table's `history.expire.*` properties and the values the
//! connection's execution and publication bounds require.

use std::collections::{BTreeMap, BTreeSet};

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{JobId, PlanId};
use chrono::{DateTime, Utc};
use iceberg::spec::{SnapshotReference, SnapshotRetention, TableMetadata, TableProperties};
use serde::{Deserialize, Serialize};

use crate::baseline::PlanningBaseline;
use crate::error::PlanError;

/// The hash domain of an expiration plan document.
const EXPIRATION_PLAN_DOMAIN: &[u8] = b"bergsmith.maintenance.v1.ExpirationPlan";

/// The expiration plan document layout version.
pub const EXPIRATION_PLAN_VERSION: u32 = 1;

/// Why a snapshot is retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionReason {
    /// The table's current snapshot.
    Current,
    /// The head of a branch or tag.
    RefHead,
    /// Pinned by a nonterminal job or an unresolved publication.
    Protected,
    /// Newer than the effective age cutoff.
    YoungerThanCutoff,
    /// Within a branch's minimum-snapshots-to-keep.
    MinimumSnapshots,
}

/// An invalid expiration policy.
#[derive(Debug, thiserror::Error)]
pub enum ExpirationError {
    /// At least one snapshot per branch must remain.
    #[error("at least one snapshot must be retained")]
    InvalidRetention,
    /// A negative age would move the cutoff into the future.
    #[error("the snapshot age bound must not be negative")]
    InvalidAge,
    /// The table properties do not decode.
    #[error("invalid table properties for snapshot expiration: {0}")]
    Properties(String),
    /// The loaded metadata does not expose its refs in the standard shape.
    #[error("invalid table metadata for snapshot expiration: {0}")]
    Metadata(String),
}

/// The retention policy applied to one table.
#[derive(Clone, Debug)]
pub struct ExpirationPolicy {
    retain_last: usize,
    max_snapshot_age_ms: i64,
    protected: BTreeSet<i64>,
}

impl ExpirationPolicy {
    /// Builds a policy from already-resolved bounds.
    ///
    /// `protected` holds snapshot identifiers the ledger pins. Identifiers not
    /// present in the metadata are ignored.
    ///
    /// # Errors
    ///
    /// [`ExpirationError::InvalidRetention`] for zero, and
    /// [`ExpirationError::InvalidAge`] for a negative age.
    pub fn new(
        retain_last: usize,
        max_snapshot_age_ms: i64,
        protected: impl IntoIterator<Item = i64>,
    ) -> Result<Self, ExpirationError> {
        if retain_last == 0 {
            return Err(ExpirationError::InvalidRetention);
        }
        if max_snapshot_age_ms < 0 {
            return Err(ExpirationError::InvalidAge);
        }
        Ok(Self {
            retain_last,
            max_snapshot_age_ms,
            protected: protected.into_iter().collect(),
        })
    }

    /// Builds a policy as the stricter of the table's properties and the
    /// connection's bounds.
    ///
    /// `configured_retain` is the operation's minimum; the table's
    /// `history.expire.min-snapshots-to-keep` can raise it. `protection_ms` is
    /// the connection's `max_execution_seconds` plus
    /// `max_unresolved_publication_seconds`; the table's
    /// `history.expire.max-snapshot-age-ms` can raise it.
    ///
    /// # Errors
    ///
    /// [`ExpirationError::Properties`] when the table's history properties do
    /// not decode, and the errors of [`Self::new`].
    pub fn from_metadata(
        metadata: &TableMetadata,
        configured_retain: usize,
        protection_ms: i64,
        protected: impl IntoIterator<Item = i64>,
    ) -> Result<Self, ExpirationError> {
        let properties = TableProperties::try_from(metadata.properties())
            .map_err(|error| ExpirationError::Properties(error.to_string()))?;
        let retain_last = configured_retain.max(properties.min_snapshots_to_keep);
        let max_snapshot_age_ms = protection_ms.max(properties.max_snapshot_age_ms);
        Self::new(retain_last, max_snapshot_age_ms, protected)
    }

    /// The effective minimum per branch.
    #[must_use]
    pub const fn retain_last(&self) -> usize {
        self.retain_last
    }

    /// The effective maximum age in milliseconds.
    #[must_use]
    pub const fn max_snapshot_age_ms(&self) -> i64 {
        self.max_snapshot_age_ms
    }

    /// The snapshot identifiers the ledger protects.
    #[must_use]
    pub fn protected(&self) -> &BTreeSet<i64> {
        &self.protected
    }
}

/// The outcome of one selection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExpirationDecision {
    /// The effective timestamp cutoff in milliseconds since the Unix epoch.
    pub cutoff_ms: i64,
    /// Snapshot identifiers that may be removed, ascending.
    pub remove: Vec<i64>,
    /// Every retained snapshot and the reason that decided it, ascending.
    pub retained: BTreeMap<i64, RetentionReason>,
}

impl ExpirationDecision {
    /// Whether the decision removes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.remove.is_empty()
    }
}

/// An immutable, hashed expiration plan (ADR 0061).
///
/// The document records the baseline the selection was made from, the
/// effective bounds, and the exact snapshot identifiers to remove and to
/// protect. It is the only input to publication, so a rerun of the planner
/// or a change to the table properties cannot change what a job commits.
///
/// The canonical JSON is the serialized typed form: field order is fixed and
/// both identifier lists are sorted and deduplicated. The hash is
/// `SHA-256("bergsmith.maintenance.v1.ExpirationPlan" || 0x00 || JSON)`.
/// PostgreSQL stores the document as `jsonb`, which reorders keys; reading it
/// back re-serializes the typed form and compares the stored hash, so the
/// stored order never matters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpirationPlanDocument {
    /// The document layout version; always [`EXPIRATION_PLAN_VERSION`].
    pub version: u32,
    /// The job, which must be in `Planning` when the document is stored.
    pub job_id: JobId,
    /// The plan's identifier.
    pub plan_id: PlanId,
    /// The table state the selection was made from.
    pub baseline: PlanningBaseline,
    /// The effective timestamp cutoff, in milliseconds since the Unix epoch.
    pub cutoff_ms: i64,
    /// The effective minimum snapshots per branch.
    pub retain_last: usize,
    /// The effective maximum snapshot age, in milliseconds.
    pub max_snapshot_age_ms: i64,
    /// Snapshot identifiers to remove, ascending and deduplicated.
    pub remove: Vec<i64>,
    /// Snapshot identifiers the ledger pinned, ascending and deduplicated.
    pub protected: Vec<i64>,
}

impl ExpirationPlanDocument {
    /// Assembles a document from a selection and the policy that produced it.
    ///
    /// Both identifier lists are sorted and deduplicated. `protected` is the
    /// policy's pin set, which may name snapshots the metadata no longer has;
    /// those identifiers are still recorded for audit, exactly as the policy
    /// was given them.
    ///
    /// # Errors
    ///
    /// [`PlanError::InvalidConfig`] when a removed snapshot is also protected:
    /// such a plan would ask the catalog to remove a snapshot the ledger pins.
    pub fn new(
        job_id: JobId,
        plan_id: PlanId,
        baseline: PlanningBaseline,
        decision: &ExpirationDecision,
        policy: &ExpirationPolicy,
    ) -> Result<Self, PlanError> {
        let mut remove = decision.remove.clone();
        remove.sort_unstable();
        remove.dedup();
        let mut protected: Vec<i64> = policy.protected.iter().copied().collect();
        protected.sort_unstable();
        protected.dedup();
        if let Some(snapshot_id) = remove
            .iter()
            .find(|snapshot_id| protected.binary_search(snapshot_id).is_ok())
        {
            return Err(PlanError::InvalidConfig(format!(
                "expiration plan removes protected snapshot {snapshot_id}"
            )));
        }
        Ok(Self {
            version: EXPIRATION_PLAN_VERSION,
            job_id,
            plan_id,
            baseline,
            cutoff_ms: decision.cutoff_ms,
            retain_last: policy.retain_last,
            max_snapshot_age_ms: policy.max_snapshot_age_ms,
            remove,
            protected,
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
            PlanError::CorruptPlan(format!("the expiration plan does not encode: {error}"))
        })
    }

    /// The document as a JSON value, for storage.
    ///
    /// # Errors
    ///
    /// As for [`to_json`](Self::to_json).
    pub fn to_value(&self) -> Result<serde_json::Value, PlanError> {
        serde_json::to_value(self).map_err(|error| {
            PlanError::CorruptPlan(format!("the expiration plan does not encode: {error}"))
        })
    }

    /// The document hash.
    ///
    /// # Errors
    ///
    /// As for [`to_json`](Self::to_json).
    pub fn content_hash(&self) -> Result<Sha256Digest, PlanError> {
        Ok(Sha256Digest::of(EXPIRATION_PLAN_DOMAIN, &self.to_json()?))
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
    /// version is not [`EXPIRATION_PLAN_VERSION`].
    pub fn from_persisted(value: serde_json::Value) -> Result<Self, PlanError> {
        let document: Self = serde_json::from_value(value).map_err(|error| {
            PlanError::CorruptPlan(format!("the expiration plan does not decode: {error}"))
        })?;
        if document.version != EXPIRATION_PLAN_VERSION {
            return Err(PlanError::CorruptPlan(format!(
                "expiration plan version {} is not supported",
                document.version
            )));
        }
        Ok(document)
    }
}

/// Reads the table's refs.
///
/// iceberg-rust exposes `snapshot_for_ref` but no iterator over the ref map, so
/// the standard metadata form provides it. The metadata was built by the same
/// crate, which makes the shape trustworthy; a malformed value is an error, not
/// a silently empty ref set.
fn refs(metadata: &TableMetadata) -> Result<BTreeMap<String, SnapshotReference>, ExpirationError> {
    let value = serde_json::to_value(metadata)
        .map_err(|error| ExpirationError::Metadata(error.to_string()))?;
    let Some(raw) = value.get("refs") else {
        return Ok(BTreeMap::new());
    };
    let object = raw
        .as_object()
        .ok_or_else(|| ExpirationError::Metadata("refs is not an object".to_owned()))?;
    object
        .iter()
        .map(|(name, reference)| {
            let reference = serde_json::from_value(reference.clone())
                .map_err(|error| ExpirationError::Metadata(error.to_string()))?;
            Ok((name.clone(), reference))
        })
        .collect()
}

/// Selects snapshots to remove from `metadata` at `now`.
///
/// # Errors
///
/// [`ExpirationError::Metadata`] when the loaded metadata has no readable ref
/// map.
pub fn plan_expiration(
    metadata: &TableMetadata,
    policy: &ExpirationPolicy,
    now: DateTime<Utc>,
) -> Result<ExpirationDecision, ExpirationError> {
    let now_ms = now.timestamp_millis();
    let cutoff_ms = now_ms.saturating_sub(policy.max_snapshot_age_ms);
    let mut retained: BTreeMap<i64, RetentionReason> = BTreeMap::new();
    if let Some(current) = metadata.current_snapshot_id() {
        retained.insert(current, RetentionReason::Current);
    }

    // Every ref head survives; only branch retention walks ancestry. Ref aging
    // is deliberately out of scope for the first release (ADR 0061).
    let mut branches: Vec<(i64, usize, i64)> = Vec::new();
    for reference in refs(metadata)?.values() {
        retained
            .entry(reference.snapshot_id)
            .or_insert(RetentionReason::RefHead);
        if let SnapshotRetention::Branch {
            min_snapshots_to_keep,
            max_snapshot_age_ms,
            ..
        } = &reference.retention
        {
            let minimum = min_snapshots_to_keep
                .map_or(policy.retain_last, |value| {
                    usize::try_from(value).unwrap_or(policy.retain_last)
                })
                .max(1);
            let branch_cutoff =
                max_snapshot_age_ms.map_or(cutoff_ms, |age| now_ms.saturating_sub(age));
            branches.push((reference.snapshot_id, minimum, branch_cutoff));
        }
    }
    // The current snapshot is a branch even when no `main` ref names it.
    if let Some(current) = metadata.current_snapshot_id()
        && !branches.iter().any(|(head, _, _)| *head == current)
    {
        branches.push((current, policy.retain_last, cutoff_ms));
    }

    for (head, minimum, branch_cutoff) in branches {
        let mut next = Some(head);
        let mut kept = 0_usize;
        while let Some(snapshot_id) = next {
            let Some(snapshot) = metadata.snapshot_by_id(snapshot_id) else {
                break;
            };
            next = snapshot.parent_snapshot_id();
            let timestamp = snapshot.timestamp_ms();
            if kept < minimum || timestamp >= branch_cutoff {
                retained
                    .entry(snapshot_id)
                    .or_insert(if timestamp >= branch_cutoff {
                        RetentionReason::YoungerThanCutoff
                    } else {
                        RetentionReason::MinimumSnapshots
                    });
                kept = kept.saturating_add(1);
            }
        }
    }

    // A snapshot no ref reaches is still retained while it is newer than the
    // default cutoff, matching Java's unreferencedSnapshotsToRetain.
    for snapshot in metadata.snapshots() {
        if snapshot.timestamp_ms() >= cutoff_ms {
            retained
                .entry(snapshot.snapshot_id())
                .or_insert(RetentionReason::YoungerThanCutoff);
        }
    }

    // A ledger pin outranks age and ancestry. Absent identifiers are ignored:
    // the snapshot was already removed, and there is nothing left to protect.
    for snapshot_id in &policy.protected {
        if metadata.snapshot_by_id(*snapshot_id).is_some() {
            retained
                .entry(*snapshot_id)
                .or_insert(RetentionReason::Protected);
        }
    }

    let mut remove: Vec<i64> = metadata
        .snapshots()
        .filter(|snapshot| !retained.contains_key(&snapshot.snapshot_id()))
        .map(|snapshot| snapshot.snapshot_id())
        .collect();
    remove.sort_unstable();
    Ok(ExpirationDecision {
        cutoff_ms,
        remove,
        retained,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::TimeZone;
    use iceberg::spec::{
        FormatVersion, NestedField, Operation, PrimitiveType, Schema, Snapshot, SnapshotReference,
        SortOrder, Summary, TableMetadataBuilder, Type, UnboundPartitionSpec,
    };

    use super::*;

    const NOW_MS: i64 = 1_700_000_000_000;
    const HOUR_MS: i64 = 60 * 60 * 1_000;

    fn now() -> DateTime<Utc> {
        Utc.timestamp_millis_opt(NOW_MS).single().unwrap()
    }

    /// A table whose snapshots form one linear history: `(id, age_hours)` are
    /// appended oldest first, and each is `age_hours` older than the next
    /// test's `now`.
    fn metadata(history: &[(i64, i64)], properties: &[(&str, &str)]) -> TableMetadata {
        let schema = Schema::builder()
            .with_fields([Arc::new(NestedField::required(
                1,
                "id",
                Type::Primitive(PrimitiveType::Long),
            ))])
            .build()
            .unwrap();
        let spec = UnboundPartitionSpec::builder().build();
        let properties = properties
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        let mut builder = TableMetadataBuilder::new(
            schema,
            spec.clone(),
            SortOrder::unsorted_order(),
            "file:///t".to_owned(),
            FormatVersion::V2,
            properties,
        )
        .unwrap()
        .assign_uuid(uuid::Uuid::now_v7());
        let mut parent = None;
        for (id, age_hours) in history {
            let snapshot = Snapshot::builder()
                .with_snapshot_id(*id)
                .with_parent_snapshot_id(parent)
                .with_sequence_number(*id)
                .with_timestamp_ms(NOW_MS - age_hours * HOUR_MS)
                .with_manifest_list(format!("file:///t/m{id}.avro"))
                .with_summary(Summary {
                    operation: Operation::Append,
                    additional_properties: HashMap::new(),
                })
                .with_schema_id(0)
                .build();
            builder = builder.set_branch_snapshot(snapshot, "main").unwrap();
            parent = Some(*id);
        }
        builder.build().unwrap().metadata
    }

    fn policy(retain: usize, age_ms: i64, protected: &[i64]) -> ExpirationPolicy {
        ExpirationPolicy::new(retain, age_ms, protected.iter().copied()).unwrap()
    }

    #[test]
    fn keeps_the_newest_and_removes_only_older_snapshots() {
        let metadata = metadata(&[(1, 6), (2, 5), (3, 4), (4, 1)], &[]);
        let decision = plan_expiration(&metadata, &policy(2, 2 * HOUR_MS, &[]), now()).unwrap();
        assert_eq!(decision.remove, vec![1, 2]);
        assert_eq!(decision.retained[&3], RetentionReason::MinimumSnapshots);
        assert_eq!(decision.retained[&4], RetentionReason::Current);
    }

    #[test]
    fn never_removes_a_ref_head_or_a_tag() {
        let mut metadata = metadata(&[(1, 9), (2, 8), (3, 1)], &[]);
        let mut builder = TableMetadataBuilder::new_from_metadata(metadata, None);
        builder = builder
            .set_ref(
                "release",
                SnapshotReference {
                    snapshot_id: 1,
                    retention: SnapshotRetention::Tag {
                        max_ref_age_ms: None,
                    },
                },
            )
            .unwrap()
            .set_ref(
                "audit",
                SnapshotReference {
                    snapshot_id: 2,
                    retention: SnapshotRetention::branch(Some(2), None, None),
                },
            )
            .unwrap();
        metadata = builder.build().unwrap().metadata;
        let decision = plan_expiration(&metadata, &policy(1, HOUR_MS, &[]), now()).unwrap();
        assert!(!decision.remove.contains(&1));
        assert!(!decision.remove.contains(&2));
        assert_eq!(decision.retained[&1], RetentionReason::RefHead);
        assert_eq!(decision.retained[&2], RetentionReason::RefHead);
        assert!(decision.remove.is_empty());
    }

    #[test]
    fn retains_a_ledger_pin_older_than_the_cutoff() {
        let metadata = metadata(&[(1, 9), (2, 8), (3, 1)], &[]);
        let decision = plan_expiration(&metadata, &policy(1, HOUR_MS, &[1]), now()).unwrap();
        assert_eq!(decision.remove, vec![2]);
        assert_eq!(decision.retained[&1], RetentionReason::Protected);
    }

    #[test]
    fn a_branch_minimum_keeps_its_ancestors() {
        let mut metadata = metadata(&[(1, 9), (2, 8), (3, 7), (4, 1)], &[]);
        let mut builder = TableMetadataBuilder::new_from_metadata(metadata, None);
        builder = builder
            .set_ref(
                "audit",
                SnapshotReference {
                    snapshot_id: 3,
                    retention: SnapshotRetention::branch(Some(2), None, None),
                },
            )
            .unwrap();
        metadata = builder.build().unwrap().metadata;
        let decision = plan_expiration(&metadata, &policy(1, HOUR_MS, &[]), now()).unwrap();
        assert_eq!(decision.remove, vec![1]);
        assert_eq!(decision.retained[&2], RetentionReason::MinimumSnapshots);
        assert_eq!(decision.retained[&3], RetentionReason::RefHead);
    }

    #[test]
    fn the_table_properties_raise_the_bounds() {
        let metadata = metadata(
            &[(1, 9), (2, 8), (3, 1)],
            &[
                ("history.expire.min-snapshots-to-keep", "3"),
                ("history.expire.max-snapshot-age-ms", "86400000"),
            ],
        );
        let resolved = ExpirationPolicy::from_metadata(&metadata, 1, HOUR_MS, []).unwrap();
        assert_eq!(resolved.retain_last(), 3);
        assert_eq!(resolved.max_snapshot_age_ms(), 86_400_000);
        // Three snapshots are within the table's minimum, so none expires.
        assert!(
            plan_expiration(&metadata, &resolved, now())
                .unwrap()
                .remove
                .is_empty()
        );
    }

    #[test]
    fn invalid_bounds_are_rejected() {
        assert!(matches!(
            ExpirationPolicy::new(0, 1, []),
            Err(ExpirationError::InvalidRetention)
        ));
        assert!(matches!(
            ExpirationPolicy::new(1, -1, []),
            Err(ExpirationError::InvalidAge)
        ));
    }

    fn document(remove: &[i64], protected: &[i64]) -> ExpirationPlanDocument {
        let decision = ExpirationDecision {
            cutoff_ms: NOW_MS - HOUR_MS,
            remove: remove.to_vec(),
            retained: BTreeMap::new(),
        };
        let policy = policy(2, HOUR_MS, protected);
        ExpirationPlanDocument::new(
            JobId::generate(),
            PlanId::generate(),
            PlanningBaseline::for_tests(),
            &decision,
            &policy,
        )
        .unwrap()
    }

    #[test]
    fn expiration_document_round_trips_with_a_stable_hash() {
        let document = document(&[1, 2], &[9]);
        let value = document.to_value().unwrap();
        let restored = ExpirationPlanDocument::from_persisted(value).unwrap();

        assert_eq!(restored, document);
        assert_eq!(
            restored.content_hash().unwrap(),
            document.content_hash().unwrap()
        );
        assert_eq!(document.version, EXPIRATION_PLAN_VERSION);
        assert_eq!(document.protected, vec![9]);
    }

    #[test]
    fn expiration_document_hash_covers_the_removed_ids() {
        let job_id = JobId::generate();
        let plan_id = PlanId::generate();
        let baseline = PlanningBaseline::for_tests();
        let policy = policy(2, HOUR_MS, &[]);
        let decision = |remove: &[i64]| ExpirationDecision {
            cutoff_ms: NOW_MS - HOUR_MS,
            remove: remove.to_vec(),
            retained: BTreeMap::new(),
        };
        let first = ExpirationPlanDocument::new(
            job_id,
            plan_id,
            baseline.clone(),
            &decision(&[1]),
            &policy,
        )
        .unwrap();
        let second =
            ExpirationPlanDocument::new(job_id, plan_id, baseline, &decision(&[2]), &policy)
                .unwrap();

        assert_ne!(
            first.content_hash().unwrap(),
            second.content_hash().unwrap()
        );
    }

    #[test]
    fn expiration_document_sorts_and_deduplicates_ids() {
        let document = document(&[3, 1, 3], &[9, 7, 9]);

        assert_eq!(document.remove, vec![1, 3]);
        assert_eq!(document.protected, vec![7, 9]);
        let value = document.to_value().unwrap();
        assert_eq!(value["remove"], serde_json::json!([1, 3]));
        assert_eq!(value["protected"], serde_json::json!([7, 9]));
    }

    #[test]
    fn expiration_document_rejects_removing_a_protected_snapshot() {
        let decision = ExpirationDecision {
            cutoff_ms: NOW_MS,
            remove: vec![1, 5],
            retained: BTreeMap::new(),
        };
        let error = ExpirationPlanDocument::new(
            JobId::generate(),
            PlanId::generate(),
            PlanningBaseline::for_tests(),
            &decision,
            &policy(1, HOUR_MS, &[5]),
        )
        .unwrap_err();

        assert!(matches!(error, PlanError::InvalidConfig(_)), "{error}");
    }

    #[test]
    fn expiration_document_rejects_unknown_fields_and_future_versions() {
        let document = document(&[1], &[]);
        let mut value = document.to_value().unwrap();
        value["unexpected"] = serde_json::json!(true);
        assert!(matches!(
            ExpirationPlanDocument::from_persisted(value),
            Err(PlanError::CorruptPlan(_))
        ));

        let mut value = document.to_value().unwrap();
        value["version"] = serde_json::json!(EXPIRATION_PLAN_VERSION + 1);
        assert!(matches!(
            ExpirationPlanDocument::from_persisted(value),
            Err(PlanError::CorruptPlan(_))
        ));
    }
}
