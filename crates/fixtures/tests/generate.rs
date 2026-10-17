//! The generator writes real, deterministic Iceberg v2 tables and refuses
//! inconsistent declarations.

// clippy's `allow-unwrap-in-tests` and `allow-panic-in-tests` cover only
// `#[test]` functions; the helpers below fail the test on setup errors.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test helpers fail the test"
)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use bergsmith_fixtures::{Commit, FileSpec, Fixture, FixtureError, TableSpec, Value, row};
use futures::TryStreamExt;
use iceberg::TableIdent;
use iceberg::spec::{
    DataContentType, Datum, FormatVersion, ManifestList, NestedField, PrimitiveType, Schema,
    Transform, Type, UnboundPartitionSpec,
};
use iceberg::table::StaticTable;

/// The reserved field ID of a position delete's `file_path` column.
const FILE_PATH_FIELD_ID: i32 = 2_147_483_546;

fn schema(fields: impl IntoIterator<Item = NestedField>) -> Schema {
    Schema::builder()
        .with_fields(fields.into_iter().map(Arc::new))
        .build()
        .unwrap()
}

fn ids() -> Schema {
    schema([NestedField::required(
        1,
        "id",
        Type::Primitive(PrimitiveType::Long),
    )])
}

/// `id long required, tag string`.
fn tagged() -> Schema {
    schema([
        NestedField::required(1, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::optional(2, "tag", Type::Primitive(PrimitiveType::String)),
    ])
}

fn tagged_rows(name: &str, count: i64) -> FileSpec {
    FileSpec::data(name, (0..count).map(|id| row![1 => id, 2 => name]))
}

/// Three data files, then a position delete and an equality delete that
/// apply to all of them.
fn with_shared_deletes() -> TableSpec {
    TableSpec::new(tagged())
        .commit(
            Commit::new()
                .file(tagged_rows("a", 3))
                .file(tagged_rows("b", 3))
                .file(tagged_rows("c", 3)),
        )
        .commit(
            Commit::new()
                .file(FileSpec::position_deletes("p", [("a", 0), ("c", 0)]))
                .file(FileSpec::equality_deletes("e", [1], [row![1 => 1_i64]])),
        )
}

/// Three commits: partitioned data, a spec change, then deletes.
fn evolving() -> TableSpec {
    let by_tag = UnboundPartitionSpec::builder()
        .add_partition_field(2, "tag", Transform::Identity)
        .unwrap()
        .build();
    TableSpec::new(tagged())
        .partitioned_by(by_tag)
        .commit(Commit::new().file(tagged_rows("x", 2).partition([Value::string("x")])))
        .commit(
            Commit::new()
                .evolve_spec(UnboundPartitionSpec::builder().build())
                .file(tagged_rows("plain", 2)),
        )
        .commit(Commit::new().file(FileSpec::position_deletes("p", [("x", 0)])))
}

/// Data, then data and an equality delete in one commit.
fn same_sequence() -> TableSpec {
    TableSpec::new(tagged())
        .commit(Commit::new().file(tagged_rows("old", 2)))
        .commit(
            Commit::new()
                .file(tagged_rows("new", 2))
                .file(FileSpec::equality_deletes("by-id", [1], [row![1 => 0_i64]])),
        )
}

/// Every Parquet file under `root`, by path relative to it.
fn parquet_files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(root.join("data"))
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read(&path).unwrap())
        })
        .collect()
}

async fn declaration_error(table: TableSpec) -> String {
    match table.generate().await.unwrap_err() {
        FixtureError::Declaration(message) => message,
        other => panic!("expected a declaration error, got {other:?}"),
    }
}

#[tokio::test]
async fn generation_is_deterministic() {
    let table = with_shared_deletes();
    let dir = tempfile::tempdir().unwrap();

    let first = table.write_to(dir.path()).await.unwrap();
    let first_files = parquet_files(dir.path());
    std::fs::remove_dir_all(dir.path().join("data")).unwrap();
    std::fs::remove_dir_all(dir.path().join("metadata")).unwrap();
    let second = table.write_to(dir.path()).await.unwrap();

    assert_eq!(first_files.len(), 5);
    assert_eq!(parquet_files(dir.path()), first_files);
    assert_eq!(second.metadata(), first.metadata());
    assert_eq!(second.metadata_location(), first.metadata_location());
}

#[tokio::test]
async fn snapshots_chain_with_sequence_numbers_in_commit_order() {
    let fixture = evolving().generate().await.unwrap();
    let metadata = fixture.metadata();

    assert_eq!(metadata.format_version(), FormatVersion::V2);
    assert_eq!(metadata.last_sequence_number(), 3);
    assert_eq!(metadata.default_partition_spec_id(), 1);
    let mut snapshot = metadata.current_snapshot().unwrap().clone();
    for expected in (1..=3).rev() {
        assert_eq!(snapshot.snapshot_id(), expected);
        assert_eq!(snapshot.sequence_number(), expected);
        match snapshot.parent_snapshot_id() {
            Some(parent) => snapshot = Arc::clone(metadata.snapshot_by_id(parent).unwrap()),
            None => assert_eq!(expected, 1),
        }
    }
}

#[tokio::test]
async fn added_files_inherit_their_commit_sequence_number() {
    let fixture = same_sequence().generate().await.unwrap();
    let file_io = fixture.file_io();
    let snapshot = fixture.metadata().current_snapshot().unwrap();
    let list = file_io
        .new_input(snapshot.manifest_list())
        .unwrap()
        .read()
        .await
        .unwrap();
    let list = ManifestList::parse_with_version(&list, FormatVersion::V2).unwrap();

    let mut sequences = BTreeMap::new();
    for manifest in list.entries() {
        for entry in manifest.load_manifest(file_io).await.unwrap().entries() {
            let name = Path::new(entry.data_file().file_path())
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            sequences.insert(
                name,
                (entry.sequence_number(), entry.data_file().content_type()),
            );
        }
    }

    assert_eq!(list.entries().len(), 3, "two new manifests and one carried");
    assert_eq!(
        sequences,
        BTreeMap::from([
            ("old".to_owned(), (Some(1), DataContentType::Data)),
            ("new".to_owned(), (Some(2), DataContentType::Data)),
            (
                "by-id".to_owned(),
                (Some(2), DataContentType::EqualityDeletes)
            ),
        ])
    );
}

#[tokio::test]
async fn position_deletes_record_their_path_bounds() {
    let fixture = with_shared_deletes().generate().await.unwrap();
    let file_io = fixture.file_io();
    let snapshot = fixture.metadata().current_snapshot().unwrap();
    let list = file_io
        .new_input(snapshot.manifest_list())
        .unwrap()
        .read()
        .await
        .unwrap();
    let list = ManifestList::parse_with_version(&list, FormatVersion::V2).unwrap();

    let mut bounds = Vec::new();
    for manifest in list.entries() {
        for entry in manifest.load_manifest(file_io).await.unwrap().entries() {
            let file = entry.data_file();
            let bound = |bounds: &std::collections::HashMap<i32, Datum>| {
                bounds.get(&FILE_PATH_FIELD_ID).cloned()
            };
            bounds.push((
                file.content_type(),
                bound(file.lower_bounds()),
                bound(file.upper_bounds()),
            ));
        }
    }

    let path = |name| Some(Datum::string(fixture.path_of(name).unwrap()));
    assert!(bounds.contains(&(DataContentType::PositionDeletes, path("a"), path("c"))));
    assert!(
        bounds
            .iter()
            .filter(|(content, ..)| *content != DataContentType::PositionDeletes)
            .all(|(_, lower, upper)| lower.is_none() && upper.is_none())
    );
}

#[tokio::test]
async fn iceberg_rust_plans_scans_of_the_generated_metadata() {
    let fixture = with_shared_deletes().generate().await.unwrap();
    let table = StaticTable::from_metadata_file(
        fixture.metadata_location(),
        TableIdent::from_strs(["db", "fixture"]).unwrap(),
        fixture.file_io().clone(),
    )
    .await
    .unwrap();

    let tasks: Vec<_> = table
        .into_table()
        .scan()
        .build()
        .unwrap()
        .plan_files()
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();

    // Both delete files are unpartitioned and newer than every data file,
    // so iceberg-rust attaches both to every task.
    assert_eq!(tasks.len(), 3);
    for task in &tasks {
        assert_eq!(task.deletes.len(), 2, "{}", task.data_file_path());
    }
}

#[tokio::test]
async fn fixture_paths_are_under_the_table_root() {
    let fixture: Fixture = TableSpec::new(ids())
        .commit(Commit::new().file(FileSpec::data("d", [row![1 => 1_i64]])))
        .generate()
        .await
        .unwrap();

    let path = fixture.path_of("d").unwrap();

    assert!(path.starts_with(&format!("file://{}", fixture.root().display())));
    assert!(path.ends_with("/data/d.parquet"));
    assert!(
        fixture
            .metadata_location()
            .ends_with("/metadata/v1.metadata.json")
    );
    assert!(matches!(
        fixture.path_of("e"),
        Err(FixtureError::Declaration(_))
    ));
}

#[tokio::test]
async fn duplicate_file_names_are_refused() {
    let table = TableSpec::new(ids()).commit(
        Commit::new()
            .file(FileSpec::data("d", [row![1 => 1_i64]]))
            .file(FileSpec::data("d", [row![1 => 2_i64]])),
    );

    assert!(declaration_error(table).await.contains("declared twice"));
}

#[tokio::test]
async fn position_deletes_must_name_an_earlier_data_file() {
    let table = TableSpec::new(ids())
        .commit(Commit::new().file(FileSpec::position_deletes("p", [("missing", 0)])));

    assert!(declaration_error(table).await.contains("missing"));
}

#[tokio::test]
async fn partition_values_must_match_the_spec() {
    let table = TableSpec::new(ids()).commit(
        Commit::new().file(FileSpec::data("d", [row![1 => 1_i64]]).partition([Value::Int(1)])),
    );

    assert!(declaration_error(table).await.contains("partition values"));
}

#[tokio::test]
async fn files_must_name_a_declared_spec() {
    let table = TableSpec::new(ids())
        .commit(Commit::new().file(FileSpec::data("d", [row![1 => 1_i64]]).in_spec(4)));

    assert!(declaration_error(table).await.contains("spec 4"));
}

#[tokio::test]
async fn required_fields_must_have_values() {
    let table = TableSpec::new(ids())
        .commit(Commit::new().file(FileSpec::data("d", [row![1 => Value::Null]])));

    assert!(declaration_error(table).await.contains("required field 1"));
}

#[tokio::test]
async fn values_must_match_the_field_type() {
    let table =
        TableSpec::new(ids()).commit(Commit::new().file(FileSpec::data("d", [row![1 => "text"]])));

    assert!(declaration_error(table).await.contains("field 1"));
}

#[tokio::test]
async fn equality_fields_must_be_in_the_schema() {
    let table = TableSpec::new(ids()).commit(Commit::new().file(FileSpec::equality_deletes(
        "e",
        [9],
        [row![9 => 1_i64]],
    )));

    assert!(declaration_error(table).await.contains("equality fields"));
}

#[tokio::test]
async fn row_groups_must_hold_rows() {
    let table = TableSpec::new(ids())
        .commit(Commit::new().file(FileSpec::data("d", [row![1 => 1_i64]]).row_groups_of(0)));

    assert!(declaration_error(table).await.contains("at least one row"));
}

#[tokio::test]
async fn initial_field_ids_must_count_from_one() {
    let table = TableSpec::new(schema([NestedField::required(
        5,
        "id",
        Type::Primitive(PrimitiveType::Long),
    )]));

    assert!(declaration_error(table).await.contains("from 1"));
}

#[tokio::test]
async fn an_evolved_schema_must_differ() {
    let table = TableSpec::new(ids()).commit(Commit::new().evolve_schema(ids()));

    assert!(declaration_error(table).await.contains("must differ"));
}
