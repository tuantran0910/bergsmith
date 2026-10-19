//! Verified, whole-file native scans (BS-010.T1).

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::time::Duration;

use bergsmith_common::digest::Sha256Digest;
use bergsmith_protocol::artifact::{DeleteContent, TaskArtifact};
use bergsmith_protocol::hashing::{artifact_digest, canonicalize_artifact};
use futures::{TryStreamExt, stream};
use iceberg::arrow::ArrowReaderBuilder;
use iceberg::io::FileIO;
use iceberg::scan::{FileScanTask, ScanResult};
use iceberg::spec::TableMetadata;
use iceberg::table::Table;
use iceberg::{Runtime, TableIdent};

use crate::baseline::PlanningBaseline;
use crate::deletes::DeleteIndex;
use crate::eligibility::{file_rejections, scope_rejections, table_rejections};
use crate::inventory::Inventory;
use crate::partition::{PartitionKey, Specs};
use crate::{PlanError, Rejection};

/// The longest metadata verification may take before row reads.
const PREPARATION_DEADLINE: Duration = Duration::from_secs(60);

/// Immutable inputs supplied by the task subprocess's caller.
///
/// `metadata` must come from the planning baseline's pinned metadata file,
/// using storage credentials, without loading a catalog or refreshing a table.
pub struct ExecutionInput<'a> {
    /// The task artifact fetched from storage.
    pub artifact: &'a TaskArtifact,
    /// The trusted digest from the durable grant, not from the downloaded file.
    pub artifact_sha256: Sha256Digest,
    /// The pinned table metadata, including the original snapshot and schemas.
    pub metadata: &'a TableMetadata,
    /// The original metadata location from the immutable plan, never a fresh head.
    pub metadata_location: &'a str,
    /// Storage access, with no catalog client or credentials.
    pub file_io: &'a FileIO,
}

/// Prepares verified input for a native rewrite inside a worker subprocess.
pub trait Executor {
    /// Rewrites verified input into the attempt prefix and returns an immutable receipt.
    ///
    /// # Errors
    ///
    /// Preparation, resource, I/O or output verification errors. Failed attempts
    /// never return a successful receipt; partial outputs are retained for
    /// coordinator cleanup. Sources are never deleted.
    fn execute(
        &self,
        request: crate::ExecutionRequest<'_>,
    ) -> impl Future<Output = Result<crate::ExecutionResult, ExecutorError>> + Send;

    /// Checks the artifact and all planned identities before permitting row reads.
    ///
    /// # Errors
    ///
    /// Returns a typed error for corrupt artifacts, mismatched metadata or files,
    /// unsupported input, metadata I/O failures, or preparation timeout.
    fn prepare(
        &self,
        input: ExecutionInput<'_>,
    ) -> impl Future<Output = Result<PreparedScan, ExecutorError>> + Send;
}

/// Bergsmith's native reader on the workspace's pinned iceberg-rust revision.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeExecutor;

/// A fully verified set of whole-file scans in canonical source-path order.
///
/// Construction is private so callers cannot bypass identity checks. Preparation
/// reads metadata only; rows are read lazily after [`Self::read`] is called.
#[must_use]
pub struct PreparedScan {
    files: Vec<FileScanTask>,
    file_io: FileIO,
    runtime: Runtime,
}

impl PreparedScan {
    /// The pinned data files and their applicable deletes, without row predicates.
    #[must_use]
    pub fn files(&self) -> &[FileScanTask] {
        &self.files
    }

    /// Opens a lazy native Arrow scan, applying iceberg-rust's delete filter.
    ///
    /// Uses the default batch size and concurrency of [`crate::ExecutionLimits`].
    /// This scan-only API is for development. Use [`Executor::execute`] to enforce
    /// attempt confinement, storage deadlines, resource budgets and receipts.
    ///
    /// # Errors
    ///
    /// iceberg-rust errors if reader construction fails. Errors during lazy file
    /// reads are returned by the resulting stream.
    pub fn read(self) -> iceberg::Result<ScanResult> {
        self.read_with_limits(&crate::ExecutionLimits::default())
    }

    pub(crate) fn read_with_limits(
        self,
        limits: &crate::ExecutionLimits,
    ) -> iceberg::Result<ScanResult> {
        ArrowReaderBuilder::new(self.file_io, self.runtime)
            .with_batch_size(limits.batch_rows)
            .with_data_file_concurrency_limit(1)
            .with_range_fetch_concurrency(limits.object_requests as usize)
            .with_metadata_size_hint(64 * 1024)
            .build()
            .read(Box::pin(stream::iter(self.files.into_iter().map(Ok))))
    }
}

/// Why task preparation or execution failed.
///
/// Execution failures may leave partial outputs for inventory and cleanup, but
/// never return a successful receipt.
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    /// At least one resource budget is zero or unrepresentable.
    #[error("invalid task execution limits")]
    InvalidLimits,
    /// A deterministic resource budget was exhausted.
    #[error("task resource limit exceeded: {0:?}")]
    ResourceLimit(crate::ResourceLimit),
    /// The supplied output root is not this job and attempt's data prefix.
    #[error("task output prefix does not match the attempt")]
    OutputPrefix,
    /// An I/O operation, including permit acquisition, exceeded its deadline.
    #[error("task storage operation timed out")]
    OperationTimeout,
    /// Parquet or result invariants contradict the planned input.
    #[error("invalid task data: {0}")]
    InvalidData(&'static str),
    /// Reading a Parquet footer failed.
    #[error("task parquet metadata could not be read")]
    Parquet(#[from] parquet::errors::ParquetError),
    /// A typed storage location is invalid.
    #[error("invalid task storage location")]
    Location(#[from] bergsmith_storage::LocationError),
    /// The downloaded artifact differs from the grant's digest.
    #[error("task artifact does not match the grant digest")]
    ArtifactDigest,
    /// A required artifact field is missing or inconsistent.
    #[error("invalid task artifact: {0}")]
    InvalidArtifact(&'static str),
    /// The supplied metadata does not match the artifact's baseline.
    #[error("table metadata does not match the task baseline")]
    BaselineMismatch,
    /// A planned data or delete file is absent or its identity changed.
    #[error("planned file identity does not match the baseline for {path}")]
    FileIdentity {
        /// The affected data or delete file.
        path: String,
    },
    /// Deletes required by a source were omitted, or an unrelated delete was added.
    #[error("task delete inventory does not match the baseline")]
    DeleteInventory,
    /// The input is outside the MVP's supported table and file features.
    #[error("task input is unsupported: {0:?}")]
    Unsupported(Vec<Rejection>),
    /// Metadata validation or inventory loading failed.
    #[error("task metadata verification failed")]
    Metadata(#[from] PlanError),
    /// Native scan planning failed.
    #[error("task storage or native scan failed")]
    Iceberg(#[from] iceberg::Error),
    /// Metadata I/O exceeded the 60-second preparation deadline.
    ///
    /// Preparation reads only metadata, bounded by the inventory limit, so an
    /// overrun indicates slow storage and suggests a fresh attempt.
    #[error("task scan preparation exceeded 60 seconds")]
    Timeout,
}

impl ExecutorError {
    /// Stable machine-readable reason for reporting and operator diagnostics.
    #[must_use]
    pub fn reason_code(&self) -> &'static str {
        match self {
            Self::ResourceLimit(limit) => limit.reason_code(),
            Self::InvalidLimits => "InvalidExecutionLimits",
            Self::OutputPrefix => "InvalidOutputPrefix",
            Self::OperationTimeout => "StorageOperationTimedOut",
            Self::ArtifactDigest => "ArtifactDigestMismatch",
            Self::InvalidArtifact(_) => "InvalidTaskArtifact",
            Self::BaselineMismatch => "PlanningBaselineMismatch",
            Self::FileIdentity { .. } | Self::DeleteInventory => "PlannedFileIdentityMismatch",
            Self::Unsupported(_) => "UnsupportedTaskInput",
            Self::InvalidData(_) | Self::Parquet(_) => "InvalidTaskData",
            Self::Metadata(PlanError::Iceberg(error)) if error.retryable() => {
                "TransientMetadataReadFailed"
            }
            Self::Metadata(_) => "MetadataVerificationFailed",
            Self::Iceberg(error) if error.retryable() => "TransientNativeExecutionFailed",
            Self::Iceberg(_) => "NativeExecutionFailed",
            Self::Location(_) => "InvalidStorageLocation",
            Self::Timeout => "PreparationTimeLimitExceeded",
        }
    }

    /// Suggested disposition; the coordinator remains authoritative.
    ///
    /// Only explicitly retryable native/storage errors, storage-operation
    /// timeouts and preparation timeouts permit a fresh attempt. Corrupt or
    /// missing inputs and resource-envelope failures, including the whole
    /// attempt's duration, are permanent.
    #[must_use]
    pub fn disposition(&self) -> bergsmith_common::disposition::FailureDisposition {
        use bergsmith_common::disposition::FailureDisposition as D;
        match self {
            Self::BaselineMismatch => D::ReplanRequired,
            Self::OperationTimeout | Self::Timeout => D::RetryTask,
            Self::Iceberg(error) | Self::Metadata(PlanError::Iceberg(error))
                if error.retryable() =>
            {
                D::RetryTask
            }
            _ => D::PermanentFailure,
        }
    }

    /// Redacted wire failure suitable for supervisor reporting.
    #[must_use]
    pub fn to_failure(&self) -> bergsmith_protocol::common::Failure {
        bergsmith_protocol::common::Failure {
            reason_code: self.reason_code().to_owned(),
            disposition: bergsmith_protocol::common::FailureDisposition::from(self.disposition())
                .into(),
            message: self.to_string(),
        }
    }
}

impl Executor for NativeExecutor {
    async fn execute(
        &self,
        request: crate::ExecutionRequest<'_>,
    ) -> Result<crate::ExecutionResult, ExecutorError> {
        crate::rewrite::execute(request).await
    }
    async fn prepare(&self, input: ExecutionInput<'_>) -> Result<PreparedScan, ExecutorError> {
        prepare_within(input, crate::ExecutionLimits::default().manifest_entries()).await
    }
}

/// Prepares `input` within the preparation deadline, reading at most
/// `max_entries` live manifest entries.
pub(crate) async fn prepare_within(
    input: ExecutionInput<'_>,
    max_entries: u64,
) -> Result<PreparedScan, ExecutorError> {
    crate::deadline::within(PREPARATION_DEADLINE, prepare(input, max_entries))
        .await
        .ok_or(ExecutorError::Timeout)?
}

async fn prepare(
    input: ExecutionInput<'_>,
    max_entries: u64,
) -> Result<PreparedScan, ExecutorError> {
    if artifact_digest(input.artifact) != input.artifact_sha256 {
        return Err(ExecutorError::ArtifactDigest);
    }
    let mut artifact = input.artifact.clone();
    canonicalize_artifact(&mut artifact);
    validate_baseline(&artifact, input.metadata)?;
    let specs = Specs::of(input.metadata)?;
    let baseline = PlanningBaseline::capture(input.metadata, Some(input.metadata_location))?
        .ok_or(ExecutorError::BaselineMismatch)?;
    let inventory = Inventory::load(
        &baseline,
        input.metadata,
        &specs,
        input.file_io,
        max_entries,
    )
    .await?;
    let delete_index = DeleteIndex::new(&inventory.deletes, &specs);
    let mut rejections = table_rejections(input.metadata);
    rejections.extend(file_rejections(&inventory, input.metadata.current_schema()));
    rejections.extend(scope_rejections(&inventory, &delete_index));
    if !rejections.is_empty() {
        return Err(ExecutorError::Unsupported(rejections));
    }
    verify_files(&artifact, &inventory, &delete_index)?;

    let runtime = Runtime::try_current()?;
    // The native planner supplies schema evolution, name mappings and original
    // partition constants. It reads manifests only. Keep only the exact planned
    // source paths; selection predicates never enter this scan.
    let table = Table::builder()
        .identifier(TableIdent::from_strs(["bergsmith", "task"])?)
        .metadata(input.metadata.clone())
        .metadata_location(input.metadata_location)
        .file_io(input.file_io.clone())
        .runtime(runtime.clone())
        .readonly(true)
        .build()?;
    let snapshot_id = baseline.snapshot_id();
    let scan = table
        .scan()
        .snapshot_id(snapshot_id)
        .select_all()
        .with_concurrency_limit(4)
        .with_manifest_entry_concurrency_limit(4)
        .build()?;
    let mut tasks = scan.plan_files().await?;
    let mut wanted: BTreeSet<&str> = artifact
        .source_files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    let mut files = Vec::with_capacity(wanted.len());
    let data: BTreeMap<_, _> = inventory
        .data
        .iter()
        .map(|f| (f.path.as_str(), f))
        .collect();
    while let Some(mut task) = tasks.try_next().await? {
        if wanted.remove(task.data_file_path.as_str()) {
            let source = data
                .get(task.data_file_path.as_str())
                .ok_or_else(|| identity(&task.data_file_path))?;
            let required: BTreeSet<_> = delete_index
                .applicable(source)
                .iter()
                .map(|d| d.file.path.as_str())
                .collect();
            let attached: BTreeSet<_> = task.deletes.iter().map(|d| d.file_path.as_str()).collect();
            if required != attached || attached.len() != task.deletes.len() {
                return Err(ExecutorError::DeleteInventory);
            }
            // A metadata-only schema evolution can leave the snapshot's schema
            // behind the baseline's schema. Rewrite into the pinned current
            // schema, resolving renamed and added columns by field ID.
            task.schema = input.metadata.current_schema().clone();
            task.project_field_ids = task
                .schema
                .as_struct()
                .fields()
                .iter()
                .map(|f| f.id)
                .collect();
            files.push(task);
        }
    }
    if let Some(path) = wanted.first() {
        return Err(identity(path));
    }
    files.sort_by(|a, b| a.data_file_path.cmp(&b.data_file_path));
    Ok(PreparedScan {
        files,
        file_io: input.file_io.clone(),
        runtime,
    })
}

fn validate_baseline(
    artifact: &TaskArtifact,
    metadata: &TableMetadata,
) -> Result<(), ExecutorError> {
    for id in [&artifact.job_id, &artifact.plan_id, &artifact.task_id] {
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(ExecutorError::InvalidArtifact(
                "invalid job, plan or task id",
            ));
        }
    }
    let table = artifact
        .table
        .as_ref()
        .ok_or(ExecutorError::InvalidArtifact("missing table"))?;
    let baseline = artifact
        .baseline
        .as_ref()
        .ok_or(ExecutorError::InvalidArtifact("missing baseline"))?;
    let output = artifact
        .output
        .as_ref()
        .ok_or(ExecutorError::InvalidArtifact("missing output"))?;
    let snapshot = metadata
        .snapshot_for_ref("main")
        .ok_or(ExecutorError::BaselineMismatch)?;
    if table.table_uuid != metadata.uuid().to_string()
        || bergsmith_storage::Location::parse(&table.location).ok()
            != bergsmith_storage::Location::parse(metadata.location()).ok()
        || table.format_version != 2
        || metadata.format_version() != iceberg::spec::FormatVersion::V2
        || baseline.snapshot_id != snapshot.snapshot_id()
        || baseline.sequence_number != snapshot.sequence_number()
        || baseline.schema_id != metadata.current_schema_id()
    {
        return Err(ExecutorError::BaselineMismatch);
    }
    let partition = output
        .partition
        .as_ref()
        .ok_or(ExecutorError::InvalidArtifact("missing output partition"))?;
    if output.partition_spec_id != partition.spec_id
        || output.data_sequence_number != baseline.sequence_number
        || output.target_file_size_bytes <= 0
        || artifact.source_files.is_empty()
    {
        return Err(ExecutorError::InvalidArtifact(
            "inconsistent output or empty sources",
        ));
    }
    Ok(())
}

fn verify_files(
    artifact: &TaskArtifact,
    inventory: &Inventory,
    index: &DeleteIndex<'_>,
) -> Result<(), ExecutorError> {
    let data: BTreeMap<_, _> = inventory
        .data
        .iter()
        .map(|f| (f.path.as_str(), f))
        .collect();
    let deletes: BTreeMap<_, _> = inventory
        .deletes
        .iter()
        .map(|d| (d.file.path.as_str(), d))
        .collect();
    let mut seen = BTreeSet::new();
    let mut required = BTreeSet::new();
    for source in &artifact.source_files {
        let file = data
            .get(source.path.as_str())
            .ok_or_else(|| identity(&source.path))?;
        if !seen.insert(&source.path)
            || source.file_size_bytes < 0
            || source.record_count < 0
            || u64::try_from(source.file_size_bytes).ok() != Some(file.size)
            || u64::try_from(source.record_count).ok() != Some(file.records)
            || source.data_sequence_number != file.sequence_number
            || source.partition != Some(partition(&file.partition))
            || source.partition.as_ref()
                != artifact.output.as_ref().and_then(|o| o.partition.as_ref())
        {
            return Err(identity(&source.path));
        }
        required.extend(index.applicable(file).iter().map(|d| d.file.path.as_str()));
    }
    let mut supplied = BTreeSet::new();
    for planned in &artifact.delete_files {
        let delete = deletes
            .get(planned.path.as_str())
            .ok_or_else(|| identity(&planned.path))?;
        let (content, mut keys, referenced) = match &delete.kind {
            crate::inventory::DeleteKind::Position { referenced, .. } => {
                (DeleteContent::Position, vec![], referenced.clone())
            }
            crate::inventory::DeleteKind::Equality { field_ids } => {
                (DeleteContent::Equality, field_ids.clone(), None)
            }
        };
        keys.sort_unstable();
        if !supplied.insert(delete.file.path.as_str())
            || u64::try_from(planned.file_size_bytes).ok() != Some(delete.file.size)
            || u64::try_from(planned.record_count).ok() != Some(delete.file.records)
            || planned.data_sequence_number != delete.file.sequence_number
            || planned.partition != Some(partition(&delete.file.partition))
            || planned.content != i32::from(content)
            || planned.equality_field_ids != keys
            || planned.referenced_data_file != referenced
        {
            return Err(identity(&planned.path));
        }
    }
    if required != supplied {
        return Err(ExecutorError::DeleteInventory);
    }
    Ok(())
}

fn identity(path: &str) -> ExecutorError {
    ExecutorError::FileIdentity {
        path: path.to_owned(),
    }
}

fn partition(key: &PartitionKey) -> bergsmith_protocol::artifact::PartitionKey {
    let mut partition = crate::plan::partition(key);
    partition.values.sort_by_key(|value| value.field_id);
    partition
}

#[cfg(test)]
mod tests {
    use super::*;
    use bergsmith_common::disposition::FailureDisposition as D;

    #[test]
    fn failures_are_classified_without_leaking_native_error_details() {
        let permanent = [
            ExecutorError::InvalidLimits,
            ExecutorError::ResourceLimit(crate::ResourceLimit::Duration),
            ExecutorError::OutputPrefix,
            ExecutorError::ArtifactDigest,
            ExecutorError::InvalidArtifact("missing output"),
            ExecutorError::FileIdentity {
                path: "memory://test/source".into(),
            },
            ExecutorError::DeleteInventory,
            ExecutorError::Unsupported(Vec::new()),
            ExecutorError::InvalidData("invalid row count"),
            ExecutorError::Parquet(parquet::errors::ParquetError::General(
                "secret-native-detail".into(),
            )),
            ExecutorError::Metadata(PlanError::CorruptPlan("secret-native-detail".into())),
            ExecutorError::Iceberg(iceberg::Error::new(
                iceberg::ErrorKind::DataInvalid,
                "secret-native-detail",
            )),
            ExecutorError::Location(bergsmith_storage::LocationError::MissingScheme(
                "secret-native-detail".into(),
            )),
        ];
        for error in permanent {
            assert_eq!(error.disposition(), D::PermanentFailure);
            let failure = error.to_failure();
            assert_eq!(failure.reason_code, error.reason_code());
            assert!(!failure.message.contains("secret-native-detail"));
        }
        assert_eq!(
            ExecutorError::BaselineMismatch.disposition(),
            D::ReplanRequired
        );
        assert_eq!(ExecutorError::OperationTimeout.disposition(), D::RetryTask);
        assert_eq!(ExecutorError::Timeout.disposition(), D::RetryTask);
        for (error, reason) in [
            (
                ExecutorError::Iceberg(
                    iceberg::Error::new(iceberg::ErrorKind::Unexpected, "temporary")
                        .with_retryable(true),
                ),
                "TransientNativeExecutionFailed",
            ),
            (
                ExecutorError::Metadata(PlanError::Iceberg(
                    iceberg::Error::new(iceberg::ErrorKind::Unexpected, "temporary")
                        .with_retryable(true),
                )),
                "TransientMetadataReadFailed",
            ),
        ] {
            assert_eq!(error.disposition(), D::RetryTask);
            assert_eq!(error.to_failure().reason_code, reason);
        }
    }
}
