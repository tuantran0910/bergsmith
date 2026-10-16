//! Each delete-application rule of the oracle, one branch per test.

// clippy's `allow-unwrap-in-tests` covers only `#[test]` functions; the
// helpers below fail the test on setup errors.
#![expect(clippy::unwrap_used, reason = "test helpers fail the test")]

use std::collections::BTreeSet;
use std::sync::Arc;

use bergsmith_fixtures::{
    Commit, FileSpec, FixtureError, Row, RowMultiset, TableSpec, Value, live_rows, row,
};
use iceberg::spec::{NestedField, PrimitiveType, Schema, Transform, Type, UnboundPartitionSpec};

const ID: i32 = 1;
const TAG: i32 = 2;

fn schema(fields: impl IntoIterator<Item = NestedField>) -> Schema {
    Schema::builder()
        .with_fields(fields.into_iter().map(Arc::new))
        .build()
        .unwrap()
}

/// `id long required, tag string`.
fn tagged() -> Schema {
    schema([
        NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::optional(TAG, "tag", Type::Primitive(PrimitiveType::String)),
    ])
}

fn identity(source: i32, name: &str) -> UnboundPartitionSpec {
    UnboundPartitionSpec::builder()
        .add_partition_field(source, name, Transform::Identity)
        .unwrap()
        .build()
}

fn rows(ids: impl IntoIterator<Item = i64>, tag: &str) -> Vec<Row> {
    ids.into_iter()
        .map(|id| row![ID => id, TAG => tag])
        .collect()
}

fn multiset(rows: Vec<Row>) -> RowMultiset {
    rows.into_iter().collect()
}

async fn live(table: TableSpec) -> RowMultiset {
    table.generate().await.unwrap().live_rows().await.unwrap()
}

async fn refusal(table: TableSpec) -> FixtureError {
    table
        .generate()
        .await
        .unwrap()
        .live_rows()
        .await
        .unwrap_err()
}

#[tokio::test]
async fn a_table_without_snapshots_has_no_rows() {
    assert!(live(TableSpec::new(tagged())).await.is_empty());
}

#[tokio::test]
async fn a_position_delete_in_the_same_commit_applies() {
    let table = TableSpec::new(tagged()).commit(
        Commit::new()
            .file(FileSpec::data("d", rows(0..3, "d")))
            .file(FileSpec::position_deletes("p", [("d", 1)])),
    );

    assert_eq!(live(table).await, multiset(rows([0, 2], "d")));
}

#[tokio::test]
async fn a_position_delete_only_removes_rows_of_its_path() {
    let table = TableSpec::new(tagged())
        .commit(
            Commit::new()
                .file(FileSpec::data("a", rows(0..2, "a")))
                .file(FileSpec::data("b", rows(0..2, "b"))),
        )
        .commit(Commit::new().file(FileSpec::position_deletes("p", [("a", 0)])));

    let expected = [rows([1], "a"), rows(0..2, "b")].concat();
    assert_eq!(live(table).await, multiset(expected));
}

#[tokio::test]
async fn an_equality_delete_skips_data_committed_after_it() {
    let table = TableSpec::new(tagged())
        .commit(Commit::new().file(FileSpec::equality_deletes("e", [ID], [row![ID => 1_i64]])))
        .commit(Commit::new().file(FileSpec::data("later", rows(0..3, "later"))));

    assert_eq!(live(table).await, multiset(rows(0..3, "later")));
}

#[tokio::test]
async fn an_equality_delete_skips_data_in_the_same_commit() {
    let table = TableSpec::new(tagged())
        .commit(Commit::new().file(FileSpec::data("old", rows(0..2, "old"))))
        .commit(
            Commit::new()
                .file(FileSpec::data("new", rows(0..2, "new")))
                .file(FileSpec::equality_deletes("e", [ID], [row![ID => 0_i64]])),
        );

    let expected = [rows([1], "old"), rows(0..2, "new")].concat();
    assert_eq!(live(table).await, multiset(expected));
}

#[tokio::test]
async fn a_null_equality_key_deletes_null_values() {
    let table = TableSpec::new(tagged())
        .commit(Commit::new().file(FileSpec::data(
            "d",
            [
                row![ID => 1_i64, TAG => Value::Null],
                row![ID => 2_i64, TAG => "x"],
            ],
        )))
        .commit(Commit::new().file(FileSpec::equality_deletes(
            "e",
            [TAG],
            [row![TAG => Value::Null]],
        )));

    assert_eq!(
        live(table).await,
        multiset(vec![row![ID => 2_i64, TAG => "x"]])
    );
}

#[tokio::test]
async fn a_partitioned_position_delete_skips_other_partitions() {
    let table = TableSpec::new(tagged())
        .partitioned_by(identity(TAG, "tag"))
        .commit(
            Commit::new()
                .file(FileSpec::data("x", rows(0..2, "x")).partition([Value::string("x")])),
        )
        .commit(
            Commit::new()
                .file(FileSpec::position_deletes("p", [("x", 0)]).partition([Value::string("y")])),
        );

    assert_eq!(live(table).await, multiset(rows(0..2, "x")));
}

#[tokio::test]
async fn an_unpartitioned_delete_applies_to_every_partition() {
    let table = TableSpec::new(tagged())
        .partitioned_by(identity(TAG, "tag"))
        .commit(
            Commit::new()
                .file(FileSpec::data("x", rows(0..2, "x")).partition([Value::string("x")]))
                .file(FileSpec::data("y", rows(0..2, "y")).partition([Value::string("y")])),
        )
        .commit(
            Commit::new()
                .evolve_spec(UnboundPartitionSpec::builder().build())
                .file(FileSpec::equality_deletes("e", [ID], [row![ID => 0_i64]])),
        );

    let expected = [rows([1], "x"), rows([1], "y")].concat();
    assert_eq!(live(table).await, multiset(expected));
}

#[tokio::test]
async fn a_partitioned_equality_delete_skips_other_specs() {
    let table = TableSpec::new(tagged())
        .commit(Commit::new().file(FileSpec::data("plain", rows(0..2, "x"))))
        .commit(
            Commit::new().evolve_spec(identity(TAG, "tag")).file(
                FileSpec::equality_deletes("e", [ID], [row![ID => 0_i64]])
                    .partition([Value::string("x")]),
            ),
        );

    assert_eq!(live(table).await, multiset(rows(0..2, "x")));
}

#[tokio::test]
async fn nan_values_are_preserved_and_counted() {
    const SCORE: i32 = 2;
    let table = TableSpec::new(schema([
        NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::optional(SCORE, "score", Type::Primitive(PrimitiveType::Double)),
    ]))
    .commit(Commit::new().file(FileSpec::data(
        "nan",
        [
            row![ID => 1_i64, SCORE => Value::double(f64::NAN)],
            row![ID => 1_i64, SCORE => Value::double(f64::NAN)],
        ],
    )));

    let live = live(table).await;

    assert_eq!(live.len(), 2);
    assert_eq!(
        live.count(&row![ID => 1_i64, SCORE => Value::double(f64::NAN)]),
        2
    );
}

#[tokio::test]
async fn floats_are_promoted_to_doubles() {
    const SCORE: i32 = 2;
    let field = |primitive| NestedField::optional(SCORE, "score", Type::Primitive(primitive));
    let id = || NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long));
    let table = TableSpec::new(schema([id(), field(PrimitiveType::Float)]))
        .commit(Commit::new().file(FileSpec::data(
            "f",
            [row![ID => 1_i64, SCORE => Value::float(1.5)]],
        )))
        .commit(Commit::new().evolve_schema(schema([id(), field(PrimitiveType::Double)])));

    assert_eq!(
        live(table).await,
        multiset(vec![row![ID => 1_i64, SCORE => Value::double(1.5)]])
    );
}

#[tokio::test]
async fn every_supported_type_round_trips() {
    let types = [
        PrimitiveType::Boolean,
        PrimitiveType::Int,
        PrimitiveType::Long,
        PrimitiveType::Float,
        PrimitiveType::Double,
        PrimitiveType::Date,
        PrimitiveType::String,
        PrimitiveType::Binary,
    ];
    let values = [
        Value::Boolean(true),
        Value::Int(-7),
        Value::Long(1 << 40),
        Value::float(0.25),
        Value::double(-2.5),
        Value::Date(20_000),
        Value::string("text"),
        Value::Binary(vec![0, 255]),
    ];
    let fields = (1..).zip(&types).map(|(id, primitive)| {
        NestedField::optional(id, format!("c{id}"), Type::Primitive(primitive.clone()))
    });
    let full = Row::new((1..).zip(values));
    let empty = Row::new((1..=8).map(|id| (id, Value::Null)));
    let table = TableSpec::new(schema(fields))
        .commit(Commit::new().file(FileSpec::data("all", [full.clone(), empty.clone()])));

    assert_eq!(live(table).await, multiset(vec![full, empty]));
}

#[tokio::test]
async fn floating_point_equality_keys_are_refused() {
    const SCORE: i32 = 2;
    let table = TableSpec::new(schema([
        NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::optional(SCORE, "score", Type::Primitive(PrimitiveType::Double)),
    ]))
    .commit(Commit::new().file(FileSpec::equality_deletes(
        "e",
        [SCORE],
        [row![SCORE => Value::double(1.0)]],
    )));

    let error = refusal(table).await;

    assert!(
        matches!(&error, FixtureError::Unsupported(message) if message.contains("floating-point")),
        "{error:?}"
    );
}

#[tokio::test]
async fn equality_keys_on_a_dropped_field_are_refused() {
    let table = TableSpec::new(tagged())
        .commit(Commit::new().file(FileSpec::equality_deletes("e", [TAG], [row![TAG => "x"]])))
        .commit(Commit::new().evolve_schema(schema([NestedField::required(
            ID,
            "id",
            Type::Primitive(PrimitiveType::Long),
        )])));

    let error = refusal(table).await;

    assert!(
        matches!(&error, FixtureError::Unsupported(message) if message.contains("lacks")),
        "{error:?}"
    );
}

#[tokio::test]
async fn nested_columns_are_refused() {
    let nested = Type::Struct(iceberg::spec::StructType::new(vec![Arc::new(
        NestedField::optional(3, "inner", Type::Primitive(PrimitiveType::Int)),
    )]));
    let table = TableSpec::new(schema([
        NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::optional(2, "outer", nested),
    ]))
    .commit(Commit::new());

    let error = refusal(table).await;

    assert!(matches!(error, FixtureError::Unsupported(_)), "{error:?}");
}

#[tokio::test]
async fn a_selection_must_name_live_data_files() {
    let fixture = TableSpec::new(tagged())
        .commit(Commit::new().file(FileSpec::data("d", rows(0..1, "d"))))
        .generate()
        .await
        .unwrap();
    let paths = BTreeSet::from(["file:///nowhere.parquet".to_owned()]);

    let error = live_rows(fixture.metadata(), fixture.file_io(), Some(&paths))
        .await
        .unwrap_err();

    assert!(matches!(error, FixtureError::Content(_)), "{error:?}");
    assert!(matches!(
        fixture.live_rows_of(&["unknown"]).await.unwrap_err(),
        FixtureError::Declaration(_)
    ));
}
