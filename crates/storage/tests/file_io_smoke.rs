//! Smoke tests for the pinned iceberg-rust `FileIO` (BS-002).
//!
//! They run against the OpenDAL memory backend, which shares the OpenDAL code
//! path used for S3. They pin down the `FileIO` behavior that `crates/storage`
//! builds on: Parquet round trips, `list`, and `delete_prefix`.

// clippy's `allow-expect-in-tests` covers only `#[test]` functions; the shared
// setup helpers below should also fail the test on any error.
#![expect(clippy::expect_used, reason = "test setup helpers fail the test")]

use std::collections::HashMap;
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

async fn write_bytes(file_io: &FileIO, path: &str) {
    file_io
        .new_output(path)
        .expect("output file")
        .write(b"x".to_vec().into())
        .await
        .expect("write succeeds");
}

async fn listed_paths(file_io: &FileIO, path: &str, recursive: bool) -> Vec<(String, bool)> {
    let mut entries: Vec<(String, bool)> = file_io
        .list(path, recursive)
        .await
        .expect("list starts")
        .map_ok(|entry| (entry.path, entry.is_dir))
        .try_collect()
        .await
        .expect("list completes");
    entries.sort();
    entries
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

#[tokio::test]
async fn recursive_list_returns_absolute_paths_of_nested_files() {
    let file_io = memory_file_io();
    let prefix = format!("{ROOT}/bergsmith/job/attempt");
    write_bytes(&file_io, &format!("{prefix}/a.parquet")).await;
    write_bytes(&file_io, &format!("{prefix}/nested/b.parquet")).await;

    let files: Vec<String> = listed_paths(&file_io, &prefix, true)
        .await
        .into_iter()
        .filter(|(_, is_dir)| !is_dir)
        .map(|(path, _)| path)
        .collect();

    assert_eq!(
        files,
        vec![
            format!("{prefix}/a.parquet"),
            format!("{prefix}/nested/b.parquet")
        ]
    );
}

#[tokio::test]
async fn non_recursive_list_returns_direct_children_only() {
    let file_io = memory_file_io();
    let prefix = format!("{ROOT}/bergsmith/job/attempt");
    write_bytes(&file_io, &format!("{prefix}/a.parquet")).await;
    write_bytes(&file_io, &format!("{prefix}/nested/b.parquet")).await;

    let entries: HashMap<String, bool> = listed_paths(&file_io, &prefix, false)
        .await
        .into_iter()
        .collect();

    assert_eq!(entries.get(&format!("{prefix}/a.parquet")), Some(&false));
    assert_eq!(entries.get(&format!("{prefix}/nested/")), Some(&true));
    assert!(!entries.contains_key(&format!("{prefix}/nested/b.parquet")));
}

#[tokio::test]
async fn list_treats_path_as_directory_not_key_prefix() {
    let file_io = memory_file_io();
    let prefix = format!("{ROOT}/bergsmith/job");
    write_bytes(&file_io, &format!("{prefix}/a.parquet")).await;
    write_bytes(&file_io, &format!("{prefix}-other/b.parquet")).await;

    let paths: Vec<String> = listed_paths(&file_io, &prefix, true)
        .await
        .into_iter()
        .map(|(path, _)| path)
        .collect();

    assert!(paths.contains(&format!("{prefix}/a.parquet")));
    assert!(paths.iter().all(|path| !path.contains("job-other")));
}

#[tokio::test]
async fn delete_prefix_removes_only_the_directory_subtree() {
    let file_io = memory_file_io();
    let prefix = format!("{ROOT}/bergsmith/job");
    let inside = format!("{prefix}/attempt/a.parquet");
    let nested = format!("{prefix}/attempt/nested/b.parquet");
    let sibling = format!("{prefix}-other/c.parquet");
    for path in [&inside, &nested, &sibling] {
        write_bytes(&file_io, path).await;
    }

    file_io
        .delete_prefix(&prefix)
        .await
        .expect("delete succeeds");

    assert!(!file_io.exists(&inside).await.expect("exists"));
    assert!(!file_io.exists(&nested).await.expect("exists"));
    assert!(file_io.exists(&sibling).await.expect("exists"));
}

#[tokio::test]
async fn delete_prefix_of_missing_path_is_a_no_op() {
    let file_io = memory_file_io();

    file_io
        .delete_prefix(format!("{ROOT}/bergsmith/missing"))
        .await
        .expect("deleting a missing prefix succeeds");
}
