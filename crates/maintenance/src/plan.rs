//! Immutable, hashed plans and their task artifacts (BS-009.T6).
//!
//! A plan document is serialized from typed structs whose fields have a
//! fixed order and whose collections are sorted, so its JSON is canonical.
//! The plan hash is `SHA-256("bergsmith.maintenance.v1.Plan" || 0x00 ||
//! JSON)`. PostgreSQL stores the document as `jsonb`, which reorders keys;
//! reading a plan back re-serializes it from the typed form and checks the
//! hash, so the stored order never matters.
//!
//! Task IDs are derived from the plan ID and the ordinal, so the same job
//! ID, plan ID, table state, selection, and configuration always give the
//! same plan and the same hash.

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{JobId, PlanId, TaskId};
use bergsmith_protocol::artifact::{self, TaskArtifact};
use bergsmith_protocol::hashing::artifact_digest;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::baseline::PlanningBaseline;
use crate::budget::Estimate;
use crate::config::PlannerConfig;
use crate::error::PlanError;
use crate::inventory::{DeleteEntry, DeleteKind, FileEntry};
use crate::partition::PartitionKey;
use crate::selection::PartitionPredicate;

const PLAN_DOMAIN: &[u8] = b"bergsmith.maintenance.v1.Plan";
const TASK_ID_DOMAIN: &[u8] = b"bergsmith.maintenance.v1.TaskId";
/// The document layout version.
const PLAN_VERSION: u32 = 1;

/// The job and plan a plan is built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanIds {
    /// The job, which must be in `Planning`.
    pub job_id: JobId,
    /// The plan's identifier.
    pub plan_id: PlanId,
}

/// A job's immutable execution plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDocument {
    version: u32,
    job_id: JobId,
    plan_id: PlanId,
    baseline: PlanningBaseline,
    config: PlannerConfig,
    selection: Vec<PartitionPredicate>,
    estimate: Estimate,
    tasks: Vec<PlannedTask>,
}

/// One task: whole data files of one `(spec, partition)` group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedTask {
    task_id: TaskId,
    ordinal: u32,
    partition: PartitionKey,
    /// Sorted by path.
    sources: Vec<PlannedFile>,
    /// The deletes that may apply to the sources, sorted by path.
    deletes: Vec<PlannedDelete>,
    /// SHA-256 of the task's [`TaskArtifact`], as lowercase hex.
    artifact_sha256: String,
}

/// A data file a task rewrites.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedFile {
    path: String,
    file_size_bytes: u64,
    record_count: u64,
    data_sequence_number: i64,
}

/// A delete file a task applies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedDelete {
    path: String,
    file_size_bytes: u64,
    record_count: u64,
    data_sequence_number: i64,
    /// Ascending; empty for a position delete.
    equality_field_ids: Vec<i32>,
    referenced_data_file: Option<String>,
    partition: PartitionKey,
}

impl PlannedFile {
    pub(crate) fn of(file: &FileEntry) -> Self {
        Self {
            path: file.path.clone(),
            file_size_bytes: file.size,
            record_count: file.records,
            data_sequence_number: file.sequence_number,
        }
    }

    /// The file's path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The file's size in bytes.
    #[must_use]
    pub const fn file_size_bytes(&self) -> u64 {
        self.file_size_bytes
    }
}

impl PlannedDelete {
    pub(crate) fn of(delete: &DeleteEntry) -> Self {
        let (mut equality_field_ids, referenced_data_file) = match &delete.kind {
            DeleteKind::Position { referenced, .. } => (Vec::new(), referenced.clone()),
            DeleteKind::Equality { field_ids } => (field_ids.clone(), None),
        };
        equality_field_ids.sort_unstable();
        Self {
            path: delete.file.path.clone(),
            file_size_bytes: delete.file.size,
            record_count: delete.file.records,
            data_sequence_number: delete.file.sequence_number,
            equality_field_ids,
            referenced_data_file,
            partition: delete.file.partition.clone(),
        }
    }

    /// The file's path.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl PlannedTask {
    /// The task's identifier.
    #[must_use]
    pub const fn task_id(&self) -> TaskId {
        self.task_id
    }

    /// The task's position in the plan, from 0.
    #[must_use]
    pub const fn ordinal(&self) -> u32 {
        self.ordinal
    }

    /// The group the task rewrites.
    #[must_use]
    pub const fn partition(&self) -> &PartitionKey {
        &self.partition
    }

    /// The data files to rewrite, sorted by path.
    #[must_use]
    pub fn sources(&self) -> &[PlannedFile] {
        &self.sources
    }

    /// The delete files to apply, sorted by path.
    #[must_use]
    pub fn deletes(&self) -> &[PlannedDelete] {
        &self.deletes
    }

    /// The SHA-256 of the task's artifact.
    ///
    /// # Errors
    ///
    /// [`PlanError::CorruptPlan`] if the stored digest is not hex.
    pub fn artifact_sha256(&self) -> Result<Sha256Digest, PlanError> {
        Sha256Digest::from_hex(&self.artifact_sha256)
            .map_err(|_| PlanError::CorruptPlan("a task artifact digest is not hex".into()))
    }
}

impl PlanDocument {
    /// Assembles a plan. `tasks` are the admitted groups in canonical
    /// order, each with its sources and deletes; task IDs and artifact
    /// digests are derived here.
    pub(crate) fn assemble(
        ids: PlanIds,
        baseline: PlanningBaseline,
        config: PlannerConfig,
        selection: Vec<PartitionPredicate>,
        estimate: Estimate,
        tasks: Vec<(PartitionKey, Vec<PlannedFile>, Vec<PlannedDelete>)>,
    ) -> Result<Self, PlanError> {
        let mut document = Self {
            version: PLAN_VERSION,
            job_id: ids.job_id,
            plan_id: ids.plan_id,
            baseline,
            config,
            selection,
            estimate,
            tasks: Vec::with_capacity(tasks.len()),
        };
        for (ordinal, (partition, mut sources, mut deletes)) in (0_u32..).zip(tasks) {
            sources.sort_by(|a, b| a.path.cmp(&b.path));
            deletes.sort_by(|a, b| a.path.cmp(&b.path));
            deletes.dedup_by(|a, b| a.path == b.path);
            let mut task = PlannedTask {
                task_id: task_id(ids.plan_id, ordinal),
                ordinal,
                partition,
                sources,
                deletes,
                artifact_sha256: String::new(),
            };
            task.artifact_sha256 = artifact_digest(&document.artifact_of(&task)?).to_hex();
            document.tasks.push(task);
        }
        Ok(document)
    }

    /// Reads a persisted plan and checks it against its hash.
    ///
    /// # Errors
    ///
    /// [`PlanError::CorruptPlan`] when the content does not decode, its hash
    /// differs from `content_hash`, or a task artifact digest does not match
    /// the task.
    pub fn from_persisted(
        content: serde_json::Value,
        content_hash: &str,
    ) -> Result<Self, PlanError> {
        let document: Self = serde_json::from_value(content).map_err(|error| {
            PlanError::CorruptPlan(format!("the plan does not decode: {error}"))
        })?;
        if document.version != PLAN_VERSION {
            return Err(PlanError::CorruptPlan(format!(
                "plan version {} is not supported",
                document.version
            )));
        }
        if document.sha256()?.to_hex() != content_hash {
            return Err(PlanError::CorruptPlan(format!(
                "plan {} does not match its hash",
                document.plan_id
            )));
        }
        for task in &document.tasks {
            if artifact_digest(&document.artifact_of(task)?) != task.artifact_sha256()? {
                return Err(PlanError::CorruptPlan(format!(
                    "task {} does not match its artifact digest",
                    task.ordinal
                )));
            }
        }
        Ok(document)
    }

    /// The canonical JSON of the plan.
    ///
    /// # Errors
    ///
    /// [`PlanError::CorruptPlan`] if serialization fails, which a plan built
    /// by the planner never does.
    pub fn to_json(&self) -> Result<Vec<u8>, PlanError> {
        serde_json::to_vec(self)
            .map_err(|error| PlanError::CorruptPlan(format!("the plan does not encode: {error}")))
    }

    /// The plan as a JSON value, for storage.
    ///
    /// # Errors
    ///
    /// As for [`to_json`](Self::to_json).
    pub fn to_value(&self) -> Result<serde_json::Value, PlanError> {
        serde_json::to_value(self)
            .map_err(|error| PlanError::CorruptPlan(format!("the plan does not encode: {error}")))
    }

    /// The plan hash.
    ///
    /// # Errors
    ///
    /// As for [`to_json`](Self::to_json).
    pub fn sha256(&self) -> Result<Sha256Digest, PlanError> {
        Ok(Sha256Digest::of(PLAN_DOMAIN, &self.to_json()?))
    }

    /// The job the plan belongs to.
    #[must_use]
    pub const fn job_id(&self) -> JobId {
        self.job_id
    }

    /// The plan's identifier.
    #[must_use]
    pub const fn plan_id(&self) -> PlanId {
        self.plan_id
    }

    /// The table state the plan was built from.
    #[must_use]
    pub const fn baseline(&self) -> &PlanningBaseline {
        &self.baseline
    }

    /// The configuration the plan was built with.
    #[must_use]
    pub const fn config(&self) -> &PlannerConfig {
        &self.config
    }

    /// The partitions the plan was limited to; empty means the whole table.
    #[must_use]
    pub fn selection(&self) -> &[PartitionPredicate] {
        &self.selection
    }

    /// The admitted work.
    #[must_use]
    pub const fn estimate(&self) -> &Estimate {
        &self.estimate
    }

    /// The tasks in ordinal order.
    #[must_use]
    pub fn tasks(&self) -> &[PlannedTask] {
        &self.tasks
    }

    /// The task IDs in ordinal order, as persistence expects them.
    #[must_use]
    pub fn task_ids(&self) -> Vec<TaskId> {
        self.tasks.iter().map(PlannedTask::task_id).collect()
    }

    /// The artifact a worker executes for `task`.
    ///
    /// # Errors
    ///
    /// [`PlanError::CorruptPlan`] when a size or count does not fit the
    /// artifact's signed fields.
    pub fn artifact_of(&self, task: &PlannedTask) -> Result<TaskArtifact, PlanError> {
        let baseline = &self.baseline;
        Ok(TaskArtifact {
            job_id: self.job_id.to_string(),
            plan_id: self.plan_id.to_string(),
            task_id: task.task_id.to_string(),
            table: Some(artifact::TableRef {
                table_uuid: baseline.table_uuid().to_string(),
                location: baseline.location().to_owned(),
                format_version: baseline.format_version(),
            }),
            baseline: Some(artifact::Baseline {
                snapshot_id: baseline.snapshot_id(),
                sequence_number: baseline.sequence_number(),
                schema_id: baseline.schema_id(),
            }),
            source_files: task
                .sources
                .iter()
                .map(|file| {
                    Ok(artifact::SourceFile {
                        path: file.path.clone(),
                        file_size_bytes: signed(file.file_size_bytes)?,
                        record_count: signed(file.record_count)?,
                        data_sequence_number: file.data_sequence_number,
                        partition: Some(partition(&task.partition)),
                    })
                })
                .collect::<Result<_, PlanError>>()?,
            delete_files: task
                .deletes
                .iter()
                .map(|delete| {
                    let content = if delete.equality_field_ids.is_empty() {
                        artifact::DeleteContent::Position
                    } else {
                        artifact::DeleteContent::Equality
                    };
                    Ok(artifact::DeleteFile {
                        path: delete.path.clone(),
                        file_size_bytes: signed(delete.file_size_bytes)?,
                        record_count: signed(delete.record_count)?,
                        data_sequence_number: delete.data_sequence_number,
                        content: content.into(),
                        equality_field_ids: delete.equality_field_ids.clone(),
                        referenced_data_file: delete.referenced_data_file.clone(),
                        partition: Some(partition(&delete.partition)),
                    })
                })
                .collect::<Result<_, PlanError>>()?,
            output: Some(artifact::OutputSpec {
                partition_spec_id: task.partition.spec_id(),
                partition: Some(partition(&task.partition)),
                target_file_size_bytes: signed(self.config.target_file_size_bytes)?,
                // Replacement files take the planning snapshot's sequence
                // number (ADR 0004).
                data_sequence_number: baseline.sequence_number(),
            }),
        })
    }
}

/// The ID of the task at `ordinal` in `plan`: a `UUIDv8` holding the first
/// 16 bytes of a domain-separated SHA-256 of the plan ID and ordinal.
fn task_id(plan: PlanId, ordinal: u32) -> TaskId {
    let mut input = plan.as_uuid().as_bytes().to_vec();
    input.extend_from_slice(&ordinal.to_be_bytes());
    let digest = Sha256Digest::of(TASK_ID_DOMAIN, &input);
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    TaskId::from_uuid(Uuid::new_v8(bytes))
}

pub(crate) fn partition(key: &PartitionKey) -> artifact::PartitionKey {
    artifact::PartitionKey {
        spec_id: key.spec_id(),
        values: key
            .values()
            .iter()
            .map(|value| artifact::PartitionValue {
                field_id: value.field_id(),
                value: value.bytes().map(<[u8]>::to_vec),
            })
            .collect(),
    }
}

fn signed(value: u64) -> Result<i64, PlanError> {
    i64::try_from(value)
        .map_err(|_| PlanError::CorruptPlan(format!("{value} does not fit a signed 64-bit field")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_ids_are_stable_v8_uuids_per_plan_and_ordinal() {
        let plan = PlanId::from_uuid(Uuid::from_u128(1));
        let other = PlanId::from_uuid(Uuid::from_u128(2));

        let first = task_id(plan, 0);

        assert_eq!(first, task_id(plan, 0));
        assert_ne!(first, task_id(plan, 1));
        assert_ne!(first, task_id(other, 0));
        assert_eq!(first.as_uuid().get_version_num(), 8);
    }

    #[test]
    fn sizes_beyond_i64_are_refused() {
        assert_eq!(signed(7).unwrap(), 7);
        assert!(matches!(signed(u64::MAX), Err(PlanError::CorruptPlan(_))));
    }
}
