//! Bounded streaming rewrites and immutable receipts (BS-010.T2–T3).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use bergsmith_common::digest::Sha256Digest;
use bergsmith_common::ids::{AttemptId, JobId};
use bergsmith_protocol::artifact::{ColumnMetrics, ExecutionMetrics, OutputFile, TaskReceipt};
use bergsmith_protocol::hashing::{canonicalize_receipt, receipt_digest};
use bergsmith_storage::Location;
use futures::TryStreamExt;
use iceberg::arrow::ArrowFileReader;
use iceberg::io::{FileIO, FileMetadata};
use iceberg::spec::{DataFile, DataFileFormat, PartitionKey};
use iceberg::writer::base_writer::data_file_writer::DataFileWriterBuilder;
use iceberg::writer::file_writer::ParquetWriterBuilder;
use iceberg::writer::file_writer::location_generator::{
    DefaultFileNameGenerator, LocationGenerator,
};
use iceberg::writer::file_writer::rolling_writer::RollingFileWriterBuilder;
use iceberg::writer::{IcebergWriter, IcebergWriterBuilder};
use parquet::arrow::async_reader::AsyncFileReader;
use parquet::basic::Compression;
use parquet::file::properties::{EnabledStatistics, WriterProperties};

use crate::execution_io::IoState;
use crate::{ExecutionInput, ExecutionLimits, ExecutorError, PreparedScan, ResourceLimit};

/// The durable identity of one grant, before result acceptance.
#[derive(Clone, Copy, Debug)]
pub struct AttemptContext {
    /// A fresh attempt ID. Never reuse accepted outputs for another attempt.
    pub attempt_id: AttemptId,
    /// Positive lease generation used to fence stale attempts.
    pub generation: i32,
}

/// A task, its storage authority and the resource envelope of one attempt.
pub struct ExecutionRequest<'a> {
    /// Pinned metadata and hashed task input.
    pub input: ExecutionInput<'a>,
    /// The persisted grant identity.
    pub attempt: AttemptContext,
    /// Exactly `<data-location>/bergsmith/<job>/<attempt>`.
    pub output_prefix: &'a Location,
    /// Fixed budgets for this attempt.
    pub limits: ExecutionLimits,
}

/// A complete attempt, ready to report but not published to a catalog.
#[must_use]
pub struct ExecutionResult {
    receipt: TaskReceipt,
    data_files: Vec<DataFile>,
}

impl ExecutionResult {
    /// The immutable, canonical receipt, with freshly computed output metrics.
    #[must_use]
    pub const fn receipt(&self) -> &TaskReceipt {
        &self.receipt
    }
    /// The domain-separated receipt digest used for idempotent reporting.
    #[must_use]
    pub fn receipt_sha256(&self) -> Sha256Digest {
        receipt_digest(&self.receipt)
    }
    /// The output descriptors, retaining the original partition spec and values.
    #[must_use]
    pub fn data_files(&self) -> &[DataFile] {
        &self.data_files
    }
}

pub(crate) async fn execute(
    request: ExecutionRequest<'_>,
) -> Result<ExecutionResult, ExecutorError> {
    request.limits.validate()?;
    if request.attempt.generation <= 0 {
        return Err(ExecutorError::InvalidArtifact(
            "attempt generation must be positive",
        ));
    }
    let baseline = crate::PlanningBaseline::capture(
        request.input.metadata,
        Some(request.input.metadata_location),
    )?
    .ok_or(ExecutorError::BaselineMismatch)?;
    let job_id: JobId = request
        .input
        .artifact
        .job_id
        .parse()
        .map_err(|_| ExecutorError::InvalidArtifact("invalid job id"))?;
    let expected = Location::parse(baseline.data_location())?
        .attempt_output_prefix(job_id, request.attempt.attempt_id);
    if &expected != request.output_prefix {
        // A well-formed prefix for this attempt under another data root was
        // derived from a baseline whose data location differs from the
        // supplied metadata's, for example after a `write.data.path` change.
        return Err(
            if request
                .output_prefix
                .is_attempt_output_prefix(job_id, request.attempt.attempt_id)
            {
                ExecutorError::BaselineMismatch
            } else {
                ExecutorError::OutputPrefix
            },
        );
    }
    let state = IoState::new(
        request.input.file_io.clone(),
        expected,
        request.limits.clone(),
    );
    let file_io = state.file_io();
    let duration = Duration::from_millis(request.limits.duration_ms);
    let result = crate::deadline::within(duration, Box::pin(run(&request, &file_io, &state)))
        .await
        .ok_or(ExecutorError::ResourceLimit(ResourceLimit::Duration))?;
    result.map_err(|error| state.error(error))
}

async fn run(
    request: &ExecutionRequest<'_>,
    file_io: &FileIO,
    state: &Arc<IoState>,
) -> Result<ExecutionResult, ExecutorError> {
    let prepared = crate::executor::prepare_within(
        ExecutionInput {
            artifact: request.input.artifact,
            artifact_sha256: request.input.artifact_sha256,
            metadata: request.input.metadata,
            metadata_location: request.input.metadata_location,
            file_io,
        },
        request.limits.manifest_entries(),
    )
    .await?;
    preflight(&prepared, file_io, &request.limits).await?;
    let first = prepared
        .files()
        .first()
        .ok_or(ExecutorError::InvalidData("no planned sources"))?;
    let schema = first.schema.clone();
    let spec = first
        .partition_spec
        .as_ref()
        .ok_or(ExecutorError::InvalidData("missing partition spec"))?;
    let partition = first
        .partition
        .clone()
        .ok_or(ExecutorError::InvalidData("missing partition values"))?;
    let key = PartitionKey::new(spec.as_ref().clone(), schema.clone(), partition);
    let output = request
        .input
        .artifact
        .output
        .as_ref()
        .ok_or(ExecutorError::InvalidArtifact("missing output"))?;
    let target = usize::try_from(output.target_file_size_bytes)
        .map_err(|_| ExecutorError::InvalidArtifact("invalid target file size"))?;
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_max_row_group_bytes(Some(request.limits.writer_row_group_bytes))
        .set_max_row_group_row_count(Some(32_768))
        .set_write_batch_size(256)
        .set_data_page_size_limit(64 * 1024)
        .set_dictionary_page_size_limit(256 * 1024)
        .set_statistics_enabled(EnabledStatistics::Chunk)
        .build();
    let rolling = RollingFileWriterBuilder::new(
        ParquetWriterBuilder::new(props, schema),
        target,
        file_io.clone(),
        AttemptLocation(request.output_prefix.clone()),
        DefaultFileNameGenerator::new("part".to_owned(), None, DataFileFormat::Parquet),
    )
    .with_max_concurrent_closes(0);
    let mut writer = DataFileWriterBuilder::new(rolling)
        .sort_order_id(Some(0))
        .build(Some(key))
        .await?;
    let scan = prepared.read_with_limits(&request.limits)?;
    let mut stream = scan.stream();
    let mut rows_written = 0_u64;
    while let Some(batch) = stream.try_next().await? {
        if batch.get_array_memory_size() as u64 > request.limits.batch_memory_bytes {
            return Err(ExecutorError::ResourceLimit(ResourceLimit::BatchMemory));
        }
        if batch.num_rows() > 0 {
            rows_written = rows_written
                .checked_add(batch.num_rows() as u64)
                .ok_or(ExecutorError::InvalidData("row count overflow"))?;
            writer.write(batch).await?;
        }
    }
    let mut data_files = writer.close().await?;
    data_files.sort_by(|a, b| a.file_path().cmp(b.file_path()));
    let receipt = receipt(request, &data_files, state, rows_written)?;
    Ok(ExecutionResult {
        receipt,
        data_files,
    })
}

#[derive(Clone)]
struct AttemptLocation(Location);

impl LocationGenerator for AttemptLocation {
    fn generate_location(&self, _: Option<&PartitionKey>, file_name: &str) -> String {
        // The native generator supplies valid single-segment names. A bad name
        // maps to the prefix itself, which the FileIO guard rejects before I/O.
        self.0
            .child(file_name)
            .map_or_else(|_| self.0.to_string(), |path| path.to_string())
    }
}

async fn preflight(
    scan: &PreparedScan,
    file_io: &FileIO,
    limits: &ExecutionLimits,
) -> Result<(), ExecutorError> {
    let mut deletes = BTreeMap::new();
    for source in scan.files() {
        let metadata = footer(
            file_io,
            &source.data_file_path,
            source.file_size_in_bytes,
            limits,
        )
        .await?;
        if u64::try_from(metadata.file_metadata().num_rows()).ok() != source.record_count {
            return Err(ExecutorError::InvalidData(
                "source footer row count differs from the plan",
            ));
        }
        for group in metadata.row_groups() {
            if u64::try_from(group.total_byte_size()).unwrap_or(u64::MAX)
                > limits.decode_row_group_bytes
            {
                return Err(ExecutorError::ResourceLimit(ResourceLimit::DecodeRowGroup));
            }
        }
        for delete in &source.deletes {
            deletes.insert(&delete.file_path, delete);
        }
    }
    let mut reservation = 0_u64;
    for delete in deletes.values() {
        let metadata = footer(
            file_io,
            &delete.file_path,
            delete.file_size_in_bytes,
            limits,
        )
        .await?;
        let count = u64::try_from(metadata.file_metadata().num_rows())
            .map_err(|_| ExecutorError::InvalidData("negative delete rows"))?;
        if Some(count) != delete.record_count {
            return Err(ExecutorError::InvalidData(
                "delete footer row count differs from the plan",
            ));
        }
        let decoded = metadata
            .row_groups()
            .iter()
            .try_fold(0_u64, |sum, group| {
                sum.checked_add(u64::try_from(group.total_byte_size()).ok()?)
            })
            .ok_or(ExecutorError::ResourceLimit(ResourceLimit::DeleteMemory))?;
        let keys = delete
            .equality_ids
            .as_ref()
            .map_or(1, |keys| keys.len().max(1)) as u64;
        // Reserve eight copies of decoded values plus 1 KiB per key and row
        // for native predicates, bound copies, caches and delete vectors.
        reservation = decoded
            .checked_mul(8)
            .and_then(|bytes| {
                count
                    .checked_mul(keys)?
                    .checked_mul(1024)?
                    .checked_add(bytes)
            })
            .and_then(|bytes| reservation.checked_add(bytes))
            .ok_or(ExecutorError::ResourceLimit(ResourceLimit::DeleteMemory))?;
        if reservation > limits.delete_memory_bytes {
            return Err(ExecutorError::ResourceLimit(ResourceLimit::DeleteMemory));
        }
    }
    Ok(())
}

async fn footer(
    file_io: &FileIO,
    path: &str,
    size: u64,
    limits: &ExecutionLimits,
) -> Result<Arc<parquet::file::metadata::ParquetMetaData>, ExecutorError> {
    let input = file_io.new_input(path)?;
    if input.metadata().await?.size != size || size < 8 {
        return Err(ExecutorError::InvalidData(
            "source object size differs from the plan",
        ));
    }
    let reader = input.reader().await?;
    let tail = reader.read(size - 8..size).await?;
    let length = tail
        .get(..4)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .map(u32::from_le_bytes)
        .ok_or(ExecutorError::InvalidData("invalid parquet footer"))?;
    if u64::from(length) > limits.footer_bytes {
        return Err(ExecutorError::ResourceLimit(ResourceLimit::Footer));
    }
    Ok(ArrowFileReader::new(FileMetadata { size }, reader)
        .get_metadata(None)
        .await?)
}

fn receipt(
    request: &ExecutionRequest<'_>,
    files: &[DataFile],
    state: &IoState,
    rows_written: u64,
) -> Result<TaskReceipt, ExecutorError> {
    let output = request
        .input
        .artifact
        .output
        .as_ref()
        .ok_or(ExecutorError::InvalidArtifact("missing output"))?;
    let rows_read = request
        .input
        .artifact
        .source_files
        .iter()
        .try_fold(0_u64, |sum, source| {
            sum.checked_add(u64::try_from(source.record_count).ok()?)
        })
        .ok_or(ExecutorError::InvalidData("invalid input row count"))?;
    let rows_deleted = rows_read
        .checked_sub(rows_written)
        .ok_or(ExecutorError::InvalidData("more output rows than input"))?;
    let mut outputs = Vec::new();
    for file in files {
        let location = Location::parse(file.file_path())?;
        if !location.is_within(request.output_prefix) || location == *request.output_prefix {
            return Err(ExecutorError::OutputPrefix);
        }
        let ids: BTreeSet<_> = file
            .value_counts()
            .keys()
            .chain(file.column_sizes().keys())
            .copied()
            .collect();
        let columns = ids
            .into_iter()
            .map(|field_id| {
                Ok(ColumnMetrics {
                    field_id,
                    column_size_bytes: signed(*file.column_sizes().get(&field_id).unwrap_or(&0))?,
                    value_count: signed(*file.value_counts().get(&field_id).unwrap_or(&0))?,
                    null_value_count: signed(
                        *file.null_value_counts().get(&field_id).unwrap_or(&0),
                    )?,
                    nan_value_count: file
                        .nan_value_counts()
                        .get(&field_id)
                        .copied()
                        .map(signed)
                        .transpose()?,
                    lower_bound: file
                        .lower_bounds()
                        .get(&field_id)
                        .map(|v| v.to_bytes().map(iceberg::spec::ByteBuf::into_vec))
                        .transpose()?,
                    upper_bound: file
                        .upper_bounds()
                        .get(&field_id)
                        .map(|v| v.to_bytes().map(iceberg::spec::ByteBuf::into_vec))
                        .transpose()?,
                })
            })
            .collect::<Result<_, ExecutorError>>()?;
        outputs.push(OutputFile {
            path: file.file_path().to_owned(),
            file_size_bytes: signed(file.file_size_in_bytes())?,
            record_count: signed(file.record_count())?,
            partition: output.partition.clone(),
            columns,
            split_offsets: file.split_offsets().unwrap_or_default().to_vec(),
        });
    }
    let sum_rows = files.iter().map(DataFile::record_count).sum::<u64>();
    let sum_bytes = files.iter().map(DataFile::file_size_in_bytes).sum::<u64>();
    let paths: Vec<_> = files.iter().map(|f| f.file_path().to_owned()).collect();
    if sum_rows != rows_written || sum_bytes != state.bytes_written() || paths != state.paths()? {
        return Err(ExecutorError::InvalidData(
            "output inventory or metrics disagree",
        ));
    }
    let mut receipt = TaskReceipt {
        task_id: request.input.artifact.task_id.clone(),
        attempt_id: request.attempt.attempt_id.to_string(),
        generation: request.attempt.generation,
        artifact_sha256: request.input.artifact_sha256.as_bytes().to_vec(),
        outputs,
        metrics: Some(ExecutionMetrics {
            rows_read: signed(rows_read)?,
            rows_deleted: signed(rows_deleted)?,
            rows_written: signed(rows_written)?,
            bytes_read: signed(state.bytes_read())?,
            bytes_written: signed(sum_bytes)?,
        }),
    };
    canonicalize_receipt(&mut receipt);
    Ok(receipt)
}

fn signed(value: u64) -> Result<i64, ExecutorError> {
    i64::try_from(value)
        .map_err(|_| ExecutorError::InvalidData("metric exceeds signed 64-bit range"))
}
