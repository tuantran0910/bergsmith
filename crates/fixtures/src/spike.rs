//! The 11 rewrite-spike fixtures: the minimum acceptance suite for the
//! executor and delete handling.
//!
//! The spike's own files were deleted after its run, so these are
//! reconstructed from its descriptions and expected row counts. Each fixture
//! declares its expected live rows without the oracle: literally for small
//! fixtures, by formula for large ones.

use std::sync::Arc;

use iceberg::spec::{NestedField, PrimitiveType, Schema, Transform, Type, UnboundPartitionSpec};

use crate::declare::{Commit, FileSpec, TableSpec};
use crate::error::{FixtureError, Result};
use crate::generate::Fixture;
use crate::row;
use crate::value::{Row, RowMultiset, Value};

/// A spike fixture: the table, the data files a rewrite group selects, and
/// the live rows of that group.
#[derive(Clone, Debug)]
pub struct SpikeFixture {
    /// A short identifier.
    pub name: &'static str,
    /// The table to generate.
    pub table: TableSpec,
    /// The data files in the rewrite group, or `None` for every data file.
    pub selection: Option<Vec<&'static str>>,
    /// The group's live rows, declared without the oracle.
    pub expected: RowMultiset,
}

impl SpikeFixture {
    /// Generates the table in a temporary directory.
    ///
    /// # Errors
    ///
    /// As for [`TableSpec::generate`].
    pub async fn generate(&self) -> Result<Fixture> {
        self.table.generate().await
    }

    /// The oracle's live rows for the selection.
    ///
    /// # Errors
    ///
    /// As for [`Fixture::live_rows_of`].
    pub async fn oracle_rows(&self, fixture: &Fixture) -> Result<RowMultiset> {
        match &self.selection {
            Some(names) => fixture.live_rows_of(names).await,
            None => fixture.live_rows().await,
        }
    }
}

/// All 11 fixtures, in the spike's order.
///
/// # Errors
///
/// [`FixtureError`] if a declaration cannot be built, which is a bug here.
pub fn all() -> Result<Vec<SpikeFixture>> {
    Ok(vec![
        small_files_with_duplicates_and_nulls()?,
        position_deletes_across_row_groups()?,
        equality_deletes_with_old_and_same_sequence_data()?,
        nullable_single_equality_key()?,
        composite_nullable_equality_keys()?,
        shared_deletes_with_an_untouched_file()?,
        fully_deleted_group()?,
        original_spec_with_a_different_default()?,
        renamed_field_and_added_column()?,
        large_equality_delete()?,
        wide_rows()?,
    ])
}

const ID: i32 = 1;
const NAME: i32 = 2;
const VALUE: i32 = 3;

/// `id long required, name string, value int`.
fn events() -> Result<Schema> {
    schema([
        NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::optional(NAME, "name", Type::Primitive(PrimitiveType::String)),
        NestedField::optional(VALUE, "value", Type::Primitive(PrimitiveType::Int)),
    ])
}

fn schema(fields: impl IntoIterator<Item = NestedField>) -> Result<Schema> {
    Ok(Schema::builder()
        .with_fields(fields.into_iter().map(Arc::new))
        .build()?)
}

fn event(id: i64, name: Option<&str>, value: Option<i32>) -> Row {
    row![ID => id, NAME => name, VALUE => value]
}

/// Ten files of four rows, with duplicates inside and across files and nulls
/// in every optional column. 40 rows.
///
/// # Errors
///
/// [`FixtureError`] if the declaration cannot be built.
pub fn small_files_with_duplicates_and_nulls() -> Result<SpikeFixture> {
    let file_rows = |file: i64| {
        let tag = format!("name-{file}");
        vec![
            event(file % 5, None, None),
            event(file % 5, None, None),
            event(
                file,
                Some(&tag),
                Some(i32::try_from(file).unwrap_or_default()),
            ),
            event(file % 2, Some("shared"), None),
        ]
    };
    let mut commit = Commit::new();
    let mut expected = RowMultiset::new();
    for file in 0..10 {
        commit = commit.file(FileSpec::data(format!("small-{file}"), file_rows(file)));
        expected.extend(file_rows(file));
    }
    Ok(SpikeFixture {
        name: "small_files_with_duplicates_and_nulls",
        table: TableSpec::new(events()?).commit(commit),
        selection: None,
        expected,
    })
}

/// 24 rows in row groups of 5, with 6 positions deleted across groups. 18
/// rows.
///
/// # Errors
///
/// [`FixtureError`] if the declaration cannot be built.
pub fn position_deletes_across_row_groups() -> Result<SpikeFixture> {
    const DELETED: [i64; 6] = [0, 4, 5, 11, 19, 23];
    let rows = |id: i64| event(id, Some("row"), Some(i32::try_from(id).unwrap_or_default()));
    Ok(SpikeFixture {
        name: "position_deletes_across_row_groups",
        table: TableSpec::new(events()?)
            .commit(
                Commit::new().file(FileSpec::data("grouped", (0..24).map(rows)).row_groups_of(5)),
            )
            .commit(Commit::new().file(FileSpec::position_deletes(
                "grouped-deletes",
                DELETED.map(|pos| ("grouped", pos)),
            ))),
        selection: None,
        expected: (0..24)
            .filter(|id| !DELETED.contains(id))
            .map(rows)
            .collect(),
    })
}

/// An equality delete applies to older data but not to data committed in
/// the same snapshot. 6 rows.
///
/// # Errors
///
/// [`FixtureError`] if the declaration cannot be built.
pub fn equality_deletes_with_old_and_same_sequence_data() -> Result<SpikeFixture> {
    let old = |id| event(id, Some("old"), None);
    let new = |id| event(id, Some("new"), None);
    Ok(SpikeFixture {
        name: "equality_deletes_with_old_and_same_sequence_data",
        table: TableSpec::new(events()?)
            .commit(Commit::new().file(FileSpec::data("old", (1..=4).map(old))))
            .commit(
                Commit::new()
                    .file(FileSpec::data("new", (1..=4).map(new)))
                    .file(FileSpec::equality_deletes(
                        "by-id",
                        [ID],
                        [row![ID => 1_i64], row![ID => 2_i64]],
                    )),
            ),
        selection: None,
        expected: [old(3), old(4), new(1), new(2), new(3), new(4)]
            .into_iter()
            .collect(),
    })
}

/// A null equality key deletes rows whose key is null. 1 row.
///
/// # Errors
///
/// [`FixtureError`] if the declaration cannot be built.
pub fn nullable_single_equality_key() -> Result<SpikeFixture> {
    Ok(SpikeFixture {
        name: "nullable_single_equality_key",
        table: TableSpec::new(events()?)
            .commit(Commit::new().file(FileSpec::data(
                "names",
                [
                    event(1, None, None),
                    event(2, Some("a"), None),
                    event(3, Some("b"), None),
                ],
            )))
            .commit(Commit::new().file(FileSpec::equality_deletes(
                "by-name",
                [NAME],
                [row![NAME => Value::Null], row![NAME => "b"]],
            ))),
        selection: None,
        expected: [event(2, Some("a"), None)].into_iter().collect(),
    })
}

/// Composite keys with nulls in either part, and same-sequence data the
/// delete must not touch. 3 rows.
///
/// # Errors
///
/// [`FixtureError`] if the declaration cannot be built.
pub fn composite_nullable_equality_keys() -> Result<SpikeFixture> {
    let key = |name: Option<&str>, value: Option<i32>| row![NAME => name, VALUE => value];
    Ok(SpikeFixture {
        name: "composite_nullable_equality_keys",
        table: TableSpec::new(events()?)
            .commit(Commit::new().file(FileSpec::data(
                "pairs",
                [
                    event(1, None, Some(1)),
                    event(2, None, None),
                    event(3, Some("x"), None),
                    event(4, Some("x"), Some(1)),
                    event(5, Some("y"), Some(2)),
                    event(6, Some("z"), Some(9)),
                ],
            )))
            .commit(
                Commit::new()
                    .file(FileSpec::data("same-sequence", [event(7, None, None)]))
                    .file(FileSpec::equality_deletes(
                        "by-pair",
                        [NAME, VALUE],
                        [
                            key(None, Some(1)),
                            key(None, None),
                            key(Some("x"), None),
                            key(Some("y"), Some(2)),
                        ],
                    )),
            ),
        selection: None,
        expected: [
            event(4, Some("x"), Some(1)),
            event(6, Some("z"), Some(9)),
            event(7, None, None),
        ]
        .into_iter()
        .collect(),
    })
}

/// Position and equality deletes shared by two selected files and an
/// untouched one. 12 selected rows.
///
/// # Errors
///
/// [`FixtureError`] if the declaration cannot be built.
pub fn shared_deletes_with_an_untouched_file() -> Result<SpikeFixture> {
    let file = |name: &str, first: i64| {
        FileSpec::data(
            name,
            (first..first + 8).map(|id| event(id, Some(name), None)),
        )
    };
    let selected = (2..8)
        .map(|id| event(id, Some("a"), None))
        .chain((12..18).map(|id| event(id, Some("b"), None)));
    Ok(SpikeFixture {
        name: "shared_deletes_with_an_untouched_file",
        table: TableSpec::new(events()?)
            .commit(
                Commit::new()
                    .file(file("a", 0))
                    .file(file("b", 10))
                    .file(file("c", 20)),
            )
            .commit(
                Commit::new()
                    .file(FileSpec::position_deletes(
                        "shared-positions",
                        [("a", 0), ("a", 1), ("c", 0)],
                    ))
                    .file(FileSpec::equality_deletes(
                        "shared-ids",
                        [ID],
                        [10_i64, 11, 21].map(|id| row![ID => id]),
                    )),
            ),
        selection: Some(vec!["a", "b"]),
        expected: selected.collect(),
    })
}

/// Every row is deleted, by position or by key. 0 rows.
///
/// # Errors
///
/// [`FixtureError`] if the declaration cannot be built.
pub fn fully_deleted_group() -> Result<SpikeFixture> {
    Ok(SpikeFixture {
        name: "fully_deleted_group",
        table: TableSpec::new(events()?)
            .commit(Commit::new().file(FileSpec::data(
                "doomed",
                (0..5).map(|id| event(id, None, None)),
            )))
            .commit(
                Commit::new()
                    .file(FileSpec::position_deletes(
                        "doomed-positions",
                        [("doomed", 0), ("doomed", 1), ("doomed", 2)],
                    ))
                    .file(FileSpec::equality_deletes(
                        "doomed-ids",
                        [ID],
                        [row![ID => 3_i64], row![ID => 4_i64]],
                    )),
            ),
        selection: None,
        expected: RowMultiset::new(),
    })
}

/// Files in spec 0, `identity(region)`, after the default became spec 1,
/// `identity(day)`. A partition-scoped delete in `eu` must not touch `us`.
/// 15 selected rows.
///
/// # Errors
///
/// [`FixtureError`] if the declaration cannot be built.
pub fn original_spec_with_a_different_default() -> Result<SpikeFixture> {
    const REGION: i32 = 2;
    const DAY: i32 = 3;
    let table = schema([
        NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::required(REGION, "region", Type::Primitive(PrimitiveType::String)),
        NestedField::optional(DAY, "day", Type::Primitive(PrimitiveType::Int)),
    ])?;
    let spec = |source, name| {
        Ok::<_, FixtureError>(
            UnboundPartitionSpec::builder()
                .add_partition_field(source, name, Transform::Identity)?
                .build(),
        )
    };
    let visit = |id: i64, region: &str, day: i32| row![ID => id, REGION => region, DAY => day];
    let region = |name: &str, count| {
        FileSpec::data(name, (1..=count).map(|id| visit(id, name, 1)))
            .partition([Value::string(name)])
    };
    Ok(SpikeFixture {
        name: "original_spec_with_a_different_default",
        table: TableSpec::new(table)
            .partitioned_by(spec(REGION, "region")?)
            .commit(Commit::new().file(region("eu", 9)).file(region("us", 7)))
            .commit(
                Commit::new().evolve_spec(spec(DAY, "day")?).file(
                    FileSpec::data("day-5", (1..=4).map(|id| visit(id, "eu", 5)))
                        .partition([Value::Int(5)]),
                ),
            )
            .commit(
                Commit::new().file(
                    FileSpec::equality_deletes("eu-first", [ID], [row![ID => 1_i64]])
                        .in_spec(0)
                        .partition([Value::string("eu")]),
                ),
            ),
        selection: Some(vec!["eu", "us"]),
        expected: (2..=9)
            .map(|id| visit(id, "eu", 1))
            .chain((1..=7).map(|id| visit(id, "us", 1)))
            .collect(),
    })
}

/// Rows written before a rename, an int to long promotion, and an added
/// optional column. Values are matched by field ID; the new column reads as
/// null. 3 selected rows.
///
/// # Errors
///
/// [`FixtureError`] if the declaration cannot be built.
pub fn renamed_field_and_added_column() -> Result<SpikeFixture> {
    const AMOUNT: i32 = 3;
    const SCORE: i32 = 4;
    let before = schema([
        NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::optional(NAME, "name", Type::Primitive(PrimitiveType::String)),
        NestedField::optional(AMOUNT, "amount", Type::Primitive(PrimitiveType::Int)),
    ])?;
    let after = schema([
        NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::optional(NAME, "full_name", Type::Primitive(PrimitiveType::String)),
        NestedField::optional(AMOUNT, "amount", Type::Primitive(PrimitiveType::Long)),
        NestedField::optional(SCORE, "score", Type::Primitive(PrimitiveType::Int)),
    ])?;
    Ok(SpikeFixture {
        name: "renamed_field_and_added_column",
        table: TableSpec::new(before)
            .commit(Commit::new().file(FileSpec::data(
                "before",
                [
                    row![ID => 1_i64, NAME => "a", AMOUNT => 10],
                    row![ID => 2_i64, NAME => Value::Null, AMOUNT => 20],
                    row![ID => 3_i64, NAME => "c", AMOUNT => Value::Null],
                ],
            )))
            .commit(Commit::new().evolve_schema(after).file(FileSpec::data(
                "after",
                [row![ID => 4_i64, NAME => "d", AMOUNT => 5_000_000_000_i64, SCORE => 7]],
            ))),
        selection: Some(vec!["before"]),
        expected: [
            row![ID => 1_i64, NAME => "a", AMOUNT => 10_i64, SCORE => Value::Null],
            row![ID => 2_i64, NAME => Value::Null, AMOUNT => 20_i64, SCORE => Value::Null],
            row![ID => 3_i64, NAME => "c", AMOUNT => Value::Null, SCORE => Value::Null],
        ]
        .into_iter()
        .collect(),
    })
}

/// 100,000 rows and 50,000 equality-delete keys, deleting every even ID.
/// 50,000 rows.
///
/// # Errors
///
/// [`FixtureError`] if the declaration cannot be built.
pub fn large_equality_delete() -> Result<SpikeFixture> {
    const ROWS: i64 = 100_000;
    let rows = |id: i64| row![ID => id, NAME => format!("row-{id}").as_str()];
    let table = schema([
        NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::optional(NAME, "name", Type::Primitive(PrimitiveType::String)),
    ])?;
    Ok(SpikeFixture {
        name: "large_equality_delete",
        table: TableSpec::new(table)
            .commit(
                Commit::new()
                    .file(FileSpec::data("large", (0..ROWS).map(rows)).row_groups_of(10_000)),
            )
            .commit(Commit::new().file(FileSpec::equality_deletes(
                "even-ids",
                [ID],
                (0..ROWS).step_by(2).map(|id| row![ID => id]),
            ))),
        selection: None,
        expected: (1..ROWS).step_by(2).map(rows).collect(),
    })
}

/// 32,768 rows with 2 KiB payloads, 64 MiB in total. 32,768 rows.
///
/// # Errors
///
/// [`FixtureError`] if the declaration cannot be built.
pub fn wide_rows() -> Result<SpikeFixture> {
    const ROWS: i64 = 32_768;
    const PAYLOAD: i64 = 2_048;
    const PAYLOAD_FIELD: i32 = 2;
    let rows = |id: i64| {
        let payload = (0..PAYLOAD)
            .map(|byte| (id * 31 + byte * 7).to_le_bytes()[0])
            .collect();
        row![ID => id, PAYLOAD_FIELD => Value::Binary(payload)]
    };
    let table = schema([
        NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::required(
            PAYLOAD_FIELD,
            "payload",
            Type::Primitive(PrimitiveType::Binary),
        ),
    ])?;
    Ok(SpikeFixture {
        name: "wide_rows",
        table: TableSpec::new(table)
            .commit(Commit::new().file(FileSpec::data("wide", (0..ROWS).map(rows)))),
        selection: None,
        expected: (0..ROWS).map(rows).collect(),
    })
}
