//! Smoke tests for the pinned iceberg-rust `FileIO` (BS-002).
//!
//! They run against the OpenDAL memory backend, which shares the OpenDAL code
//! path used for S3.

// clippy's `allow-expect-in-tests` covers only `#[test]` functions; the shared
// setup helpers below should also fail the test on any error.
#![expect(clippy::expect_used, reason = "test setup helpers fail the test")]

use std::sync::Arc;

use arrow_array::{Int64Array, RecordBatch, StringArray};
use futures::TryStreamExt;
use iceberg::arrow::{ArrowFileReader, schema_to_arrow_schema};
use iceberg::io::{FileIO, FileIOBuilder};
use iceberg::spec::{NestedField, PrimitiveType, Schema, Type};
use iceberg::writer::file_writer::{FileWriter, FileWriterBuilder, ParquetWriterBuilder};
use iceberg_storage_opendal::OpenDalStorageFactory;
use parquet::arrow::ParquetRecordBatchStreamBuilder;
use parquet::file::properties::WriterProperties;

const ROOT: &str = "memory:/warehouse/db/table/data";

fn memory_file_io() -> FileIO {
    FileIOBuilder::new(Arc::new(OpenDalStorageFactory::memory())).build()
}

fn table_schema() -> Schema {
    Schema::builder()
        .with_fields(vec![
            NestedField::required(1, "id", Type::Primitive(PrimitiveType::Long)).into(),
            NestedField::optional(2, "name", Type::Primitive(PrimitiveType::String)).into(),
        ])
        .build()
        .expect("schema is valid")
}

fn sample_batch(schema: &Schema) -> RecordBatch {
    let arrow_schema = schema_to_arrow_schema(schema).expect("schema converts to arrow");
    RecordBatch::try_new(
        Arc::new(arrow_schema),
        vec![
            Arc::new(Int64Array::from(vec![1, 2, 3])),
            Arc::new(StringArray::from(vec![Some("a"), None, Some("c")])),
        ],
    )
    .expect("batch matches schema")
}

#[tokio::test]
async fn parquet_data_file_round_trips_through_file_io() {
    let file_io = memory_file_io();
    let schema = table_schema();
    let batch = sample_batch(&schema);
    let path = format!("{ROOT}/part-00000.parquet");

    let builder = ParquetWriterBuilder::new(WriterProperties::builder().build(), Arc::new(schema));
    let mut writer = builder
        .build(file_io.new_output(&path).expect("output file"))
        .await
        .expect("writer builds");
    writer.write(&batch).await.expect("batch writes");
    let data_files = writer.close().await.expect("writer closes");
    assert_eq!(data_files.len(), 1);

    let input = file_io.new_input(&path).expect("input file");
    let reader = ArrowFileReader::new(
        input.metadata().await.expect("metadata"),
        input.reader().await.expect("reader"),
    );
    let batches: Vec<RecordBatch> = ParquetRecordBatchStreamBuilder::new(reader)
        .await
        .expect("parquet footer reads")
        .build()
        .expect("stream builds")
        .try_collect()
        .await
        .expect("batches read");

    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].columns(), batch.columns());
}
