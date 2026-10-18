//! Metadata-only verification and atomic acceptance of native task results (BS-012.T3).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use bergsmith_common::ids::{AttemptId, JobId, TaskId};
use bergsmith_maintenance::{PlanDocument, PlanError};
use bergsmith_persistence::{PersistenceError, attempts, publications};
use bergsmith_protocol::artifact::{TaskArtifact, TaskReceipt};
use bergsmith_protocol::hashing::{canonicalize_receipt, receipt_digest};
use bergsmith_storage::{Location, LocationError, ObjectStore, StorageError};
use iceberg::arrow::ArrowFileReader;
use iceberg::io::FileMetadata;
use iceberg::spec::{DataFile, DataFileFormat, Datum, Literal, Struct, TableMetadata, Type};
use parquet::arrow::async_reader::AsyncFileReader;
use prost::Message;
use sqlx::PgConnection;

use crate::planning;

const IO_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FOOTER_BYTES: u32 = 16 * 1024 * 1024;

/// A result cannot authorize publication until its identity, objects, and metadata agree.
#[derive(Debug, thiserror::Error)]
pub enum ResultError {
    /// A receipt or stored result disagrees with the plan or object metadata.
    #[error("task result failed verification: {0}")]
    Invalid(&'static str),
    /// A durable ledger operation failed.
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    /// A stored plan failed verification.
    #[error(transparent)]
    Planning(#[from] planning::PlanningError),
    /// A plan's artifact could not be reconstructed.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// A location is invalid.
    #[error(transparent)]
    Location(#[from] LocationError),
    /// Object metadata could not be read.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// Parquet or manifest metadata could not be read.
    #[error("output metadata could not be read")]
    Iceberg(#[from] iceberg::Error),
    /// Output Parquet metadata could not be decoded.
    #[error("output parquet metadata could not be read")]
    Parquet(#[from] parquet::errors::ParquetError),
    /// An object-store operation exceeded its deadline.
    #[error("task result verification timed out")]
    Timeout,
}

pub(crate) async fn plan(conn: &mut PgConnection, job: JobId) -> Result<PlanDocument, ResultError> {
    planning::load_plan(conn, job)
        .await?
        .ok_or(ResultError::Invalid("job has no plan"))
}

/// Records an attempt prefix before execution and verifies that it is empty.
///
/// The returned location is the only output authority for this attempt. A crash
/// after registration leaves the prefix inventoried, including incomplete writes.
///
/// # Errors
/// Rejects stale grants, unknown tasks, reused nonempty prefixes, or storage failures.
pub async fn register_attempt_outputs(
    conn: &mut PgConnection,
    attempt_id: AttemptId,
    store: &ObjectStore,
) -> Result<Location, ResultError> {
    let attempt = attempts::attempt(conn, attempt_id).await?;
    let task = bergsmith_persistence::plans::task(conn, attempt.task_id).await?;
    let plan = plan(conn, task.job_id).await?;
    if !plan.task_ids().contains(&task.id) {
        return Err(ResultError::Invalid("task is absent from plan"));
    }
    let prefix = Location::parse(plan.baseline().data_location())?
        .attempt_output_prefix(task.job_id, attempt_id);
    publications::register_prefix(conn, attempt_id, &prefix.to_string()).await?;
    let objects = tokio::time::timeout(IO_TIMEOUT, store.list(&prefix))
        .await
        .map_err(|_| ResultError::Timeout)??;
    if !objects.is_empty() {
        return Err(ResultError::Invalid("attempt output prefix is not empty"));
    }
    Ok(prefix)
}

/// Verifies finalized output metadata without reading table rows, then accepts it durably.
///
/// Exact accepted replays return the stored disposition without requiring objects
/// to remain readable. Acceptance still serializes against cancellation and expiry
/// after storage I/O. Zero-output receipts complete a task normally.
///
/// # Errors
/// Rejects identity/hash/partition/metric mismatches, missing or extra objects,
/// invalid Parquet metadata, unregistered prefixes, and stale attempts.
pub async fn accept_result(
    conn: &mut PgConnection,
    receipt: &TaskReceipt,
    store: &ObjectStore,
    metadata: &TableMetadata,
) -> Result<attempts::Acceptance, ResultError> {
    let attempt_id = receipt
        .attempt_id
        .parse()
        .map_err(|_| ResultError::Invalid("invalid attempt id"))?;
    let attempt = attempts::attempt(conn, attempt_id).await?;
    let task = bergsmith_persistence::plans::task(conn, attempt.task_id).await?;
    let mut canonical = receipt.clone();
    canonicalize_receipt(&mut canonical);
    let submission = attempts::ReceiptSubmission {
        attempt_id,
        generation: receipt.generation,
        content: serde_json::json!(canonical.encode_to_vec()),
        content_hash: receipt_digest(receipt).to_hex(),
    };
    let existing = publications::accepted_receipts(conn, task.job_id).await?;
    if existing.iter().any(|r| r.attempt_id == attempt_id) {
        return Ok(attempts::accept_receipt(conn, &submission).await?);
    }
    let plan = plan(conn, task.job_id).await?;
    if attempt.generation != receipt.generation || receipt.task_id != task.id.to_string() {
        return Err(ResultError::Invalid("receipt does not match grant"));
    }
    let verified = verify_receipt(&plan, &canonical, store, metadata).await?;
    Ok(publications::accept_outputs(
        conn,
        &submission,
        &verified.prefix.to_string(),
        &verified.objects,
    )
    .await?)
}

/// Output metadata verified without a database connection.
pub(crate) struct VerifiedReceipt {
    pub(crate) prefix: Location,
    pub(crate) objects: Vec<(String, i64)>,
}

pub(crate) async fn verify_receipt(
    plan: &PlanDocument,
    receipt: &TaskReceipt,
    store: &ObjectStore,
    metadata: &TableMetadata,
) -> Result<VerifiedReceipt, ResultError> {
    let attempt_id = receipt
        .attempt_id
        .parse()
        .map_err(|_| ResultError::Invalid("invalid attempt id"))?;
    let task_id: TaskId = receipt
        .task_id
        .parse()
        .map_err(|_| ResultError::Invalid("invalid task id"))?;
    let planned = plan
        .tasks()
        .iter()
        .find(|t| t.task_id() == task_id)
        .ok_or(ResultError::Invalid("task is absent from plan"))?;
    let artifact = plan.artifact_of(planned)?;
    let prefix = Location::parse(plan.baseline().data_location())?
        .attempt_output_prefix(plan.job_id(), attempt_id);
    tokio::time::timeout(
        IO_TIMEOUT,
        verify(&artifact, receipt, &prefix, store, metadata),
    )
    .await
    .map_err(|_| ResultError::Timeout)??;
    let objects = receipt
        .outputs
        .iter()
        .map(|o| (o.path.clone(), o.file_size_bytes))
        .collect();
    Ok(VerifiedReceipt { prefix, objects })
}

pub(crate) fn decode(receipt: &publications::AcceptedReceipt) -> Result<TaskReceipt, ResultError> {
    let bytes: Vec<u8> = serde_json::from_value(receipt.content.clone())
        .map_err(|_| ResultError::Invalid("stored receipt encoding"))?;
    let decoded = TaskReceipt::decode(bytes.as_slice())
        .map_err(|_| ResultError::Invalid("stored receipt protobuf"))?;
    if receipt_digest(&decoded).to_hex() != receipt.content_hash
        || decoded.attempt_id != receipt.attempt_id.to_string()
    {
        return Err(ResultError::Invalid("stored receipt digest or identity"));
    }
    Ok(decoded)
}

pub(crate) async fn verify(
    artifact: &TaskArtifact,
    receipt: &TaskReceipt,
    prefix: &Location,
    store: &ObjectStore,
    metadata: &TableMetadata,
) -> Result<Vec<DataFile>, ResultError> {
    if receipt.task_id != artifact.task_id
        || receipt.generation <= 0
        || receipt.artifact_sha256
            != bergsmith_protocol::hashing::artifact_digest(artifact).as_bytes()
    {
        return Err(ResultError::Invalid("receipt task or artifact identity"));
    }
    let baseline = artifact
        .baseline
        .as_ref()
        .ok_or(ResultError::Invalid("missing artifact baseline"))?;
    if metadata.uuid().to_string()
        != artifact
            .table
            .as_ref()
            .ok_or(ResultError::Invalid("missing table"))?
            .table_uuid
        || metadata.current_schema_id() != baseline.schema_id
    {
        return Err(ResultError::Invalid("wrong output schema or table"));
    }
    let expected_partition = artifact
        .output
        .as_ref()
        .and_then(|o| o.partition.as_ref())
        .ok_or(ResultError::Invalid("missing output partition"))?;
    let mut partition = expected_partition.clone();
    partition.values.sort_by_key(|v| v.field_id);
    let mut paths = BTreeSet::new();
    let mut files = Vec::new();
    for output in &receipt.outputs {
        let path = Location::parse(&output.path)?;
        if !path.is_within(prefix)
            || path == *prefix
            || path.to_string() != output.path
            || !paths.insert(output.path.clone())
        {
            return Err(ResultError::Invalid("duplicate or out-of-prefix output"));
        }
        files.push(verify_output(output, &partition, store, metadata).await?);
    }
    let listed = store.list(prefix).await?;
    let actual: BTreeMap<_, _> = listed
        .into_iter()
        .map(|o| (o.location.to_string(), o.size))
        .collect();
    let expected: BTreeMap<_, _> = files
        .iter()
        .map(|f| (f.file_path().to_owned(), f.file_size_in_bytes()))
        .collect();
    if actual != expected {
        return Err(ResultError::Invalid("finalized object inventory"));
    }
    let metrics = receipt
        .metrics
        .as_ref()
        .ok_or(ResultError::Invalid("missing execution metrics"))?;
    let rows_read = artifact
        .source_files
        .iter()
        .try_fold(0_i64, |sum, f| sum.checked_add(f.record_count))
        .ok_or(ResultError::Invalid("input rows overflow"))?;
    let rows_written = receipt
        .outputs
        .iter()
        .try_fold(0_i64, |sum, f| sum.checked_add(f.record_count))
        .ok_or(ResultError::Invalid("output rows overflow"))?;
    let bytes_written = receipt
        .outputs
        .iter()
        .try_fold(0_i64, |sum, f| sum.checked_add(f.file_size_bytes))
        .ok_or(ResultError::Invalid("output bytes overflow"))?;
    if metrics.rows_read != rows_read
        || metrics.rows_written != rows_written
        || metrics.bytes_written != bytes_written
        || metrics.rows_deleted < 0
        || metrics.rows_deleted.checked_add(rows_written) != Some(rows_read)
        || metrics.bytes_read < 0
    {
        return Err(ResultError::Invalid("execution metrics"));
    }
    files.sort_by(|a, b| a.file_path().cmp(b.file_path()));
    Ok(files)
}

fn unsigned(value: i64) -> Result<u64, ResultError> {
    u64::try_from(value).map_err(|_| ResultError::Invalid("negative metric"))
}

async fn verify_output(
    output: &bergsmith_protocol::artifact::OutputFile,
    partition: &bergsmith_protocol::artifact::PartitionKey,
    store: &ObjectStore,
    metadata: &TableMetadata,
) -> Result<DataFile, ResultError> {
    let mut actual_partition = output
        .partition
        .clone()
        .ok_or(ResultError::Invalid("missing output partition"))?;
    actual_partition.values.sort_by_key(|v| v.field_id);
    if actual_partition != *partition || output.file_size_bytes <= 0 || output.record_count <= 0 {
        return Err(ResultError::Invalid("output partition, size, or rows"));
    }
    let footer = read_footer(output, store, metadata).await?;
    let (sizes, values, nulls, offsets, lower, upper, nan) =
        column_metrics(output, &footer, metadata)?;
    let literals = partition_literals(partition, metadata)?;
    let metrics = |map: BTreeMap<i32, i64>| -> Result<HashMap<i32, u64>, ResultError> {
        map.into_iter()
            .map(|(id, n)| Ok((id, unsigned(n)?)))
            .collect()
    };
    iceberg::spec::DataFileBuilder::default()
        .content(iceberg::spec::DataContentType::Data)
        .file_path(output.path.clone())
        .file_format(DataFileFormat::Parquet)
        .partition(Struct::from_iter(literals))
        .partition_spec_id(partition.spec_id)
        .record_count(unsigned(output.record_count)?)
        .file_size_in_bytes(unsigned(output.file_size_bytes)?)
        .column_sizes(metrics(sizes)?)
        .value_counts(metrics(values)?)
        .null_value_counts(metrics(nulls)?)
        .nan_value_counts(nan)
        .lower_bounds(lower)
        .upper_bounds(upper)
        .split_offsets(Some(offsets))
        .sort_order_id(0)
        .build()
        .map_err(|_| ResultError::Invalid("invalid output descriptor"))
}

async fn read_footer(
    output: &bergsmith_protocol::artifact::OutputFile,
    store: &ObjectStore,
    metadata: &TableMetadata,
) -> Result<std::sync::Arc<parquet::file::metadata::ParquetMetaData>, ResultError> {
    let input = store.file_io().new_input(&output.path)?;
    let size = input.metadata().await?.size;
    if i64::try_from(size).ok() != Some(output.file_size_bytes) || size < 8 {
        return Err(ResultError::Invalid("output object size"));
    }
    let reader = input.reader().await?;
    let tail = reader.read(size - 8..size).await?;
    let length = tail
        .get(..4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes)
        .ok_or(ResultError::Invalid("parquet footer"))?;
    if tail.get(4..) != Some(b"PAR1") || length > MAX_FOOTER_BYTES || u64::from(length) > size - 8 {
        return Err(ResultError::Invalid("invalid or oversized parquet footer"));
    }
    let footer = ArrowFileReader::new(FileMetadata { size }, reader)
        .get_metadata(None)
        .await?;
    if footer.file_metadata().num_rows() != output.record_count {
        return Err(ResultError::Invalid("output footer row count"));
    }
    let expected_schema = iceberg::arrow::schema_to_arrow_schema(metadata.current_schema())?;
    let arrow = parquet::arrow::parquet_to_arrow_schema(
        footer.file_metadata().schema_descr(),
        footer.file_metadata().key_value_metadata(),
    )
    .map_err(|_| ResultError::Invalid("parquet schema"))?;
    // Arrow's Parquet round trip may add encoding metadata; compare fields and IDs.
    if arrow.fields() != expected_schema.fields() {
        return Err(ResultError::Invalid("output footer schema or field ids"));
    }
    Ok(footer)
}

fn partition_literals(
    partition: &bergsmith_protocol::artifact::PartitionKey,
    metadata: &TableMetadata,
) -> Result<Vec<Option<Literal>>, ResultError> {
    let spec = metadata
        .partition_spec_by_id(partition.spec_id)
        .ok_or(ResultError::Invalid("unknown output spec"))?;
    let partition_type = bind_partition_type(spec, metadata.schemas_iter())?;
    let mut literals = Vec::new();
    for (field, nested) in spec.fields().iter().zip(partition_type.fields()) {
        let value = partition
            .values
            .iter()
            .find(|v| v.field_id == field.field_id)
            .ok_or(ResultError::Invalid("missing partition field"))?;
        let Type::Primitive(primitive) = nested.field_type.as_ref() else {
            return Err(ResultError::Invalid("nonprimitive partition"));
        };
        literals.push(
            value
                .value
                .as_ref()
                .map(|b| {
                    Datum::try_from_bytes(b, primitive.clone())
                        .map(|d| Literal::Primitive(d.literal().clone()))
                })
                .transpose()?,
        );
    }
    Ok(literals)
}

fn bind_partition_type<'a>(
    spec: &iceberg::spec::PartitionSpec,
    schemas: impl Iterator<Item = &'a iceberg::spec::SchemaRef>,
) -> Result<iceberg::spec::StructType, ResultError> {
    // Match the planner's newest-compatible-schema binding. TableMetadata's
    // schema map has no iteration order, and promoted partition values must
    // decode using the newest retained type that can bind the entire spec.
    let mut schemas: Vec<_> = schemas.collect();
    schemas.sort_by_key(|schema| std::cmp::Reverse(schema.schema_id()));
    schemas
        .into_iter()
        .find_map(|schema| spec.partition_type(schema).ok())
        .ok_or(ResultError::Invalid("unbound output spec"))
}

type ColumnMaps = (
    BTreeMap<i32, i64>,
    BTreeMap<i32, i64>,
    BTreeMap<i32, i64>,
    Vec<i64>,
    HashMap<i32, Datum>,
    HashMap<i32, Datum>,
    HashMap<i32, u64>,
);
fn column_metrics(
    output: &bergsmith_protocol::artifact::OutputFile,
    footer: &parquet::file::metadata::ParquetMetaData,
    metadata: &TableMetadata,
) -> Result<ColumnMaps, ResultError> {
    let mut sizes = BTreeMap::<i32, i64>::new();
    let mut values = BTreeMap::<i32, i64>::new();
    let mut nulls = BTreeMap::<i32, i64>::new();
    let mut offsets = Vec::new();
    for group in footer.row_groups() {
        for column in group.columns() {
            let descriptor = column.column_descr();
            let basic = descriptor.self_type().get_basic_info();
            if !basic.has_id() {
                return Err(ResultError::Invalid("missing parquet field id"));
            }
            let id = basic.id();
            *sizes.entry(id).or_default() += column.compressed_size();
            *values.entry(id).or_default() += column.num_values();
            if let Some(count) = column
                .statistics()
                .and_then(parquet::file::statistics::Statistics::null_count_opt)
            {
                *nulls.entry(id).or_default() += i64::try_from(count)
                    .map_err(|_| ResultError::Invalid("null count overflow"))?;
            }
        }
        if let Some(offset) = group.file_offset() {
            offsets.push(offset);
        }
    }
    if offsets != output.split_offsets {
        return Err(ResultError::Invalid("output split offsets"));
    }
    let mut seen = BTreeSet::new();
    let mut lower = HashMap::new();
    let mut upper = HashMap::new();
    let mut nan = HashMap::new();
    for column in &output.columns {
        let id = column.field_id;
        if !seen.insert(id)
            || sizes.get(&id) != Some(&column.column_size_bytes)
            || values.get(&id) != Some(&column.value_count)
            || nulls.get(&id).copied().unwrap_or(0) != column.null_value_count
            || column
                .nan_value_count
                .is_some_and(|n| n < 0 || n > column.value_count)
        {
            return Err(ResultError::Invalid("output column metrics"));
        }
        let field = metadata
            .current_schema()
            .field_by_id(id)
            .ok_or(ResultError::Invalid("unknown metric field"))?;
        let Type::Primitive(primitive) = field.field_type.as_ref() else {
            return Err(ResultError::Invalid("nonprimitive metric field"));
        };
        if let Some(bytes) = &column.lower_bound {
            lower.insert(id, Datum::try_from_bytes(bytes, primitive.clone())?);
        }
        if let Some(bytes) = &column.upper_bound {
            upper.insert(id, Datum::try_from_bytes(bytes, primitive.clone())?);
        }
        if let Some(count) = column.nan_value_count {
            nan.insert(id, unsigned(count)?);
        }
    }
    if seen.len() != sizes.len() {
        return Err(ResultError::Invalid("missing column metrics"));
    }
    let (footer_lower, footer_upper) = footer_bounds(footer, metadata)?;
    if lower != footer_lower || upper != footer_upper {
        return Err(ResultError::Invalid("output column bounds"));
    }
    Ok((sizes, values, nulls, offsets, lower, upper, nan))
}

type Bounds = HashMap<i32, Datum>;

fn footer_bounds(
    footer: &parquet::file::metadata::ParquetMetaData,
    metadata: &TableMetadata,
) -> Result<(Bounds, Bounds), ResultError> {
    let mut lower = Bounds::new();
    let mut upper = Bounds::new();
    for group in footer.row_groups() {
        for column in group.columns() {
            let id = column.column_descr().self_type().get_basic_info().id();
            let field = metadata
                .current_schema()
                .field_by_id(id)
                .ok_or(ResultError::Invalid("unknown footer field"))?;
            let Type::Primitive(primitive) = field.field_type.as_ref() else {
                return Err(ResultError::Invalid("nonprimitive footer field"));
            };
            if let Some(stats) = column.statistics() {
                for (minimum, map) in [(true, &mut lower), (false, &mut upper)] {
                    if let Some(datum) = statistic_bound(stats, primitive, minimum)?
                        && map.get(&id).is_none_or(|previous| {
                            if minimum {
                                datum < *previous
                            } else {
                                datum > *previous
                            }
                        })
                    {
                        map.insert(id, datum);
                    }
                }
            }
        }
    }
    Ok((lower, upper))
}

fn statistic_bound(
    stats: &parquet::file::statistics::Statistics,
    primitive: &iceberg::spec::PrimitiveType,
    minimum: bool,
) -> Result<Option<Datum>, ResultError> {
    use iceberg::spec::PrimitiveType;
    use parquet::file::statistics::Statistics;
    if !(if minimum {
        stats.min_is_exact()
    } else {
        stats.max_is_exact()
    }) {
        return Ok(None);
    }
    // Decimal integer physical encodings are little endian; Iceberg decimals
    // use signed big-endian bytes, including when the Parquet type is INT32/64.
    let decimal = if matches!(primitive, PrimitiveType::Decimal { .. }) {
        match stats {
            Statistics::Int32(s) => (if minimum { s.min_opt() } else { s.max_opt() })
                .map(|v| i128::from(*v).to_be_bytes()),
            Statistics::Int64(s) => (if minimum { s.min_opt() } else { s.max_opt() })
                .map(|v| i128::from(*v).to_be_bytes()),
            _ => None,
        }
    } else {
        None
    };
    let bytes = decimal.as_ref().map(<[u8; 16]>::as_slice).or_else(|| {
        if minimum {
            stats.min_bytes_opt()
        } else {
            stats.max_bytes_opt()
        }
    });
    bytes
        .map(|b| Datum::try_from_bytes(b, primitive.clone()).map_err(ResultError::from))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bergsmith_common::ids::{JobId, PlanId};
    use bergsmith_fixtures::spike;
    use bergsmith_maintenance::{
        AttemptContext, ExecutionInput, ExecutionLimits, ExecutionRequest, Executor,
        NativeExecutor, PlanIds, PlanInput, PlanOutcome, Planner, PlannerConfig,
    };
    use bergsmith_protocol::hashing::artifact_digest;
    use bergsmith_storage::StorageProfile;

    #[test]
    fn partition_type_promotion_is_independent_of_retained_schema_order() {
        use iceberg::spec::{NestedField, PrimitiveType, Schema, Transform, UnboundPartitionSpec};
        let schema = |id, primitive| {
            std::sync::Arc::new(
                Schema::builder()
                    .with_schema_id(id)
                    .with_fields([std::sync::Arc::new(NestedField::required(
                        1,
                        "id",
                        Type::Primitive(primitive),
                    ))])
                    .build()
                    .unwrap(),
            )
        };
        let old = schema(0, PrimitiveType::Int);
        let new = schema(1, PrimitiveType::Long);
        let spec = UnboundPartitionSpec::builder()
            .add_partition_field(1, "id_partition", Transform::Identity)
            .unwrap()
            .build()
            .bind(old.clone())
            .unwrap();
        for schemas in [[&old, &new], [&new, &old]] {
            let bound = bind_partition_type(&spec, schemas.into_iter()).unwrap();
            assert_eq!(
                bound.fields()[0].field_type.as_ref(),
                &Type::Primitive(PrimitiveType::Long)
            );
        }
    }

    fn promoted_partition_fixture() -> spike::SpikeFixture {
        use bergsmith_fixtures::{Commit, FileSpec, Row, TableSpec, Value};
        use iceberg::spec::{NestedField, PrimitiveType, Schema, Transform, UnboundPartitionSpec};
        let schema = |primitive| {
            Schema::builder()
                .with_fields([std::sync::Arc::new(NestedField::required(
                    1,
                    "id",
                    Type::Primitive(primitive),
                ))])
                .build()
                .unwrap()
        };
        let row = Row::new([(1, Value::Long((1_i64 << 40) + 7))]);
        spike::SpikeFixture {
            name: "promoted_identity_partition",
            table: TableSpec::new(schema(PrimitiveType::Int))
                .partitioned_by(
                    UnboundPartitionSpec::builder()
                        .add_partition_field(1, "id_partition", Transform::Identity)
                        .unwrap()
                        .build(),
                )
                .commit(Commit::new())
                .commit(
                    Commit::new()
                        .evolve_schema(schema(PrimitiveType::Long))
                        .file(
                            FileSpec::data("promoted", [row.clone()])
                                .partition([Value::Long((1_i64 << 40) + 7)]),
                        ),
                ),
            selection: None,
            expected: [row].into_iter().collect(),
        }
    }

    #[tokio::test]
    async fn verified_descriptors_match_native_outputs_across_small_oracle_fixtures() {
        let cases = [
            spike::small_files_with_duplicates_and_nulls().unwrap(),
            spike::position_deletes_across_row_groups().unwrap(),
            spike::nullable_single_equality_key().unwrap(),
            spike::composite_nullable_equality_keys().unwrap(),
            spike::original_spec_with_a_different_default().unwrap(),
            spike::renamed_field_and_added_column().unwrap(),
            spike::fully_deleted_group().unwrap(),
            promoted_partition_fixture(),
        ];
        for case in cases {
            let fixture = case.generate().await.unwrap();
            let job_id = JobId::generate();
            let outcome = Planner::new(PlannerConfig {
                min_input_files: 1,
                min_files_removed: 0,
                ..PlannerConfig::default()
            })
            .unwrap()
            .plan(
                PlanInput {
                    metadata: fixture.metadata(),
                    metadata_location: Some(fixture.metadata_location()),
                    file_io: fixture.file_io(),
                    selection: &[],
                },
                PlanIds {
                    job_id,
                    plan_id: PlanId::generate(),
                },
            )
            .await
            .unwrap();
            let PlanOutcome::Planned(plan) = outcome else {
                panic!("expected planned rewrite");
            };
            let store = ObjectStore::open(&StorageProfile::LocalFs, None).unwrap();
            for task in plan.tasks() {
                let artifact = plan.artifact_of(task).unwrap();
                let attempt_id = AttemptId::generate();
                let prefix = Location::parse(plan.baseline().data_location())
                    .unwrap()
                    .attempt_output_prefix(job_id, attempt_id);
                let result = NativeExecutor
                    .execute(ExecutionRequest {
                        input: ExecutionInput {
                            artifact: &artifact,
                            artifact_sha256: artifact_digest(&artifact),
                            metadata: fixture.metadata(),
                            metadata_location: fixture.metadata_location(),
                            file_io: fixture.file_io(),
                        },
                        attempt: AttemptContext {
                            attempt_id,
                            generation: 1,
                        },
                        output_prefix: &prefix,
                        limits: ExecutionLimits::default(),
                    })
                    .await
                    .unwrap();
                let descriptors = verify(
                    &artifact,
                    result.receipt(),
                    &prefix,
                    &store,
                    fixture.metadata(),
                )
                .await
                .unwrap();
                assert_eq!(descriptors, result.data_files(), "{}", case.name);
            }
        }
    }
}
