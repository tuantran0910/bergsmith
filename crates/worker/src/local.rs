//! Execute an immutable plan locally, with storage-only authority.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bergsmith_common::ids::AttemptId;
use bergsmith_maintenance::{
    AttemptContext, ExecutionInput, ExecutionLimits, ExecutionRequest, Executor, ExecutorError,
    NativeExecutor, PlanDocument, PlanError, PlanningBaseline,
};
use bergsmith_storage::{Backend, Location, ObjectStore, StorageError, StorageProfile};
use prost::Message;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

/// Inputs to local execution. No catalog connection or database is needed.
pub struct LocalRequest {
    /// Canonical plan JSON produced by the planner.
    pub plan_file: PathBuf,
    /// Trusted plan SHA-256, supplied separately from the downloaded document.
    pub plan_sha256: String,
    /// A new directory for immutable protobuf receipts and `summary.json`.
    /// Existing directories are refused so previous runs cannot be overwritten.
    pub receipt_dir: PathBuf,
    /// Optional JSON resource envelope. Omitted fields use execution defaults.
    pub limits_file: Option<PathBuf>,
}

/// Summary of one task's local execution, without catalog publication.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalReceipt {
    /// Task identifier from the immutable plan.
    pub task_id: String,
    /// Fresh attempt identifier for this execution.
    pub attempt_id: String,
    /// SHA-256 of the protobuf receipt.
    pub receipt_sha256: String,
    /// Location of the immutable receipt file.
    pub receipt_file: PathBuf,
    /// Input rows before delete application.
    pub rows_read: i64,
    /// Output live rows.
    pub rows_written: i64,
    /// Actual storage bytes read, including preparation and footer preflight.
    pub bytes_read: i64,
    /// Physical output bytes written.
    pub bytes_written: i64,
}

/// A completed local execution. Outputs are not published to a catalog.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalRun {
    /// Measured wall time for metadata loading, rewrites and receipt persistence.
    pub elapsed_millis: u64,
    /// Task results in immutable plan order.
    pub receipts: Vec<LocalReceipt>,
}

/// Why local execution could not complete.
#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    /// Reading input or exclusively creating an output failed.
    #[error("local execution file operation failed")]
    Io(#[from] std::io::Error),
    /// Input JSON is not a valid plan or limit document.
    #[error("local execution json is invalid")]
    Json(#[from] serde_json::Error),
    /// The immutable plan is corrupt or mismatched.
    #[error("local execution plan verification failed")]
    Plan(#[from] PlanError),
    /// Native execution failed. The typed reason and disposition are retained.
    #[error("local task execution failed: {0}")]
    Execution(#[from] ExecutorError),
    /// Building local storage failed.
    #[error("local execution storage could not be opened")]
    Storage(#[from] StorageError),
    /// Reading pinned metadata failed.
    #[error("local execution metadata could not be read")]
    Iceberg(#[from] iceberg::Error),
    /// A pinned location is invalid.
    #[error("local execution location is invalid")]
    Location(#[from] bergsmith_storage::LocationError),
    /// Local mode is restricted to filesystem metadata and table data.
    #[error("local execution requires filesystem locations")]
    NonLocal,
    /// The metadata is not the complete planning baseline.
    #[error("local execution metadata differs from the planning baseline")]
    BaselineMismatch,
}

/// Executes every task in a verified plan, sequentially in the current process.
///
/// Receipts are protobuf files created exclusively under a new receipt directory.
/// Output Parquet files use fresh attempt prefixes under the planned data root.
/// A failure retains outputs and earlier receipts for inspection; it never
/// deletes source data or publishes metadata. Process isolation and credential
/// refresh belong to the distributed supervisor (BS-015).
///
/// # Errors
///
/// [`LocalError`] for invalid plans, changed baselines, non-local storage,
/// existing receipt directories, I/O failures or typed execution failures.
pub async fn run(request: LocalRequest) -> Result<LocalRun, LocalError> {
    let start = Instant::now();
    let content = serde_json::from_slice(&tokio::fs::read(&request.plan_file).await?)?;
    let plan = PlanDocument::from_persisted(content, &request.plan_sha256)?;
    let limits: ExecutionLimits = match &request.limits_file {
        Some(path) => serde_json::from_slice(&tokio::fs::read(path).await?)?,
        None => ExecutionLimits::default(),
    };
    limits.validate()?;
    let baseline = plan.baseline();
    for path in [
        baseline.metadata_location(),
        baseline.location(),
        baseline.data_location(),
    ] {
        if Location::parse(path)?.backend() != Backend::LocalFs {
            return Err(LocalError::NonLocal);
        }
    }
    let storage = ObjectStore::open(&StorageProfile::LocalFs, None)?;
    let bytes = tokio::time::timeout(Duration::from_millis(limits.io_timeout_ms), async {
        storage
            .file_io()
            .new_input(baseline.metadata_location())?
            .read()
            .await
    })
    .await
    .map_err(|_| ExecutorError::OperationTimeout)??;
    let metadata = serde_json::from_slice(&bytes)?;
    if PlanningBaseline::capture(&metadata, Some(baseline.metadata_location()))?.as_ref()
        != Some(baseline)
    {
        return Err(LocalError::BaselineMismatch);
    }
    tokio::fs::create_dir(&request.receipt_dir).await?;
    let mut receipts = Vec::with_capacity(plan.tasks().len());
    for task in plan.tasks() {
        let artifact = plan.artifact_of(task)?;
        let attempt_id = AttemptId::from_uuid(Uuid::now_v7());
        let prefix = Location::parse(baseline.data_location())?
            .attempt_output_prefix(plan.job_id(), attempt_id);
        let result = NativeExecutor
            .execute(ExecutionRequest {
                input: ExecutionInput {
                    artifact: &artifact,
                    artifact_sha256: task.artifact_sha256()?,
                    metadata: &metadata,
                    metadata_location: baseline.metadata_location(),
                    file_io: storage.file_io(),
                },
                attempt: AttemptContext {
                    attempt_id,
                    generation: 1,
                },
                output_prefix: &prefix,
                limits: limits.clone(),
            })
            .await?;
        let receipt = result.receipt();
        let receipt_file = request.receipt_dir.join(format!("{}.pb", task.ordinal()));
        create(&receipt_file, &receipt.encode_to_vec()).await?;
        let metrics = receipt
            .metrics
            .as_ref()
            .ok_or(ExecutorError::InvalidData("missing execution metrics"))?;
        receipts.push(LocalReceipt {
            task_id: receipt.task_id.clone(),
            attempt_id: receipt.attempt_id.clone(),
            receipt_sha256: result.receipt_sha256().to_hex(),
            receipt_file,
            rows_read: metrics.rows_read,
            rows_written: metrics.rows_written,
            bytes_read: metrics.bytes_read,
            bytes_written: metrics.bytes_written,
        });
    }
    let run = LocalRun {
        elapsed_millis: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        receipts,
    };
    create(
        &request.receipt_dir.join("summary.json"),
        &serde_json::to_vec_pretty(&run)?,
    )
    .await?;
    Ok(run)
}

async fn create(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    file.write_all(bytes).await?;
    file.sync_all().await
}
