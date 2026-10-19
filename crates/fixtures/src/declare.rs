//! Declaring a fixture table: its schema, partitioning, and commits.

use iceberg::spec::{Schema, UnboundPartitionSpec};

use crate::value::{Row, Value};

/// A table to generate: an initial schema and partition spec, then commits
/// applied in order.
///
/// Commit `i` (from 0) gets snapshot ID `i + 1`, sequence number `i + 1`,
/// and a timestamp one minute after the previous commit, so generation is
/// deterministic. Schema IDs and partition-spec IDs count up from 0 in the
/// order they are declared.
#[derive(Clone, Debug)]
pub struct TableSpec {
    pub(crate) schema: Schema,
    pub(crate) spec: UnboundPartitionSpec,
    pub(crate) commits: Vec<Commit>,
    pub(crate) column_metrics: bool,
}

impl TableSpec {
    /// An unpartitioned table. Field IDs must count up from 1 in declaration
    /// order, because new table metadata renumbers them otherwise.
    #[must_use]
    pub fn new(schema: Schema) -> Self {
        Self {
            schema,
            spec: UnboundPartitionSpec::builder().build(),
            commits: Vec::new(),
            column_metrics: false,
        }
    }

    /// Uses `spec` as the initial partition spec, ID 0.
    #[must_use]
    pub fn partitioned_by(mut self, spec: UnboundPartitionSpec) -> Self {
        self.spec = spec;
        self
    }

    /// Records column metrics in every data and equality-delete file's
    /// manifest entry, as engines such as Spark and Flink do: value, null,
    /// and NaN counts and exact lower and upper bounds for each top-level
    /// column. Without it, only position deletes record (`file_path`) bounds.
    #[must_use]
    pub const fn with_column_metrics(mut self) -> Self {
        self.column_metrics = true;
        self
    }

    /// Appends a commit.
    #[must_use]
    pub fn commit(mut self, commit: Commit) -> Self {
        self.commits.push(commit);
        self
    }
}

/// One snapshot: optional schema and spec changes, then the files it adds.
#[derive(Clone, Debug, Default)]
pub struct Commit {
    pub(crate) schema: Option<Schema>,
    pub(crate) spec: Option<UnboundPartitionSpec>,
    pub(crate) files: Vec<FileSpec>,
}

impl Commit {
    /// A commit that changes nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes `schema` current before this commit's files are written. It
    /// gets the next schema ID.
    #[must_use]
    pub fn evolve_schema(mut self, schema: Schema) -> Self {
        self.schema = Some(schema);
        self
    }

    /// Makes `spec` the default before this commit's files are written. It
    /// gets the next partition-spec ID.
    #[must_use]
    pub fn evolve_spec(mut self, spec: UnboundPartitionSpec) -> Self {
        self.spec = Some(spec);
        self
    }

    /// Adds `file`.
    #[must_use]
    pub fn file(mut self, file: FileSpec) -> Self {
        self.files.push(file);
        self
    }
}

/// One data or delete file. `name` identifies it within the fixture and
/// becomes `<location>/data/<name>.parquet`.
#[derive(Clone, Debug)]
pub struct FileSpec {
    pub(crate) name: String,
    pub(crate) spec_id: Option<i32>,
    pub(crate) partition: Vec<Value>,
    pub(crate) content: Content,
    pub(crate) row_group_rows: Option<usize>,
    pub(crate) omitted_fields: Vec<i32>,
}

/// What a file holds.
#[derive(Clone, Debug)]
pub(crate) enum Content {
    Data(Vec<Row>),
    PositionDeletes(Vec<(String, i64)>),
    EqualityDeletes { field_ids: Vec<i32>, keys: Vec<Row> },
}

impl FileSpec {
    /// A data file with `rows`, written with the commit's current schema. A
    /// field a row omits is written as null.
    #[must_use]
    pub fn data(name: impl Into<String>, rows: impl IntoIterator<Item = Row>) -> Self {
        Self::with_content(name, Content::Data(rows.into_iter().collect()))
    }

    /// A position-delete file. Each entry names a data file of this fixture
    /// and a zero-based row position in it.
    #[must_use]
    pub fn position_deletes<'a>(
        name: impl Into<String>,
        deletes: impl IntoIterator<Item = (&'a str, i64)>,
    ) -> Self {
        let deletes = deletes
            .into_iter()
            .map(|(file, pos)| (file.to_owned(), pos))
            .collect();
        Self::with_content(name, Content::PositionDeletes(deletes))
    }

    /// An equality-delete file on `field_ids`. Each key row holds a value
    /// for every one of those fields; an omitted field is null.
    #[must_use]
    pub fn equality_deletes(
        name: impl Into<String>,
        field_ids: impl IntoIterator<Item = i32>,
        keys: impl IntoIterator<Item = Row>,
    ) -> Self {
        Self::with_content(
            name,
            Content::EqualityDeletes {
                field_ids: field_ids.into_iter().collect(),
                keys: keys.into_iter().collect(),
            },
        )
    }

    /// Places the file in the commit's default spec with these partition
    /// values, one per partition field.
    #[must_use]
    pub fn partition(mut self, values: impl IntoIterator<Item = Value>) -> Self {
        self.partition = values.into_iter().collect();
        self
    }

    /// Places the file in partition spec `spec_id` instead of the default.
    #[must_use]
    pub fn in_spec(mut self, spec_id: i32) -> Self {
        self.spec_id = Some(spec_id);
        self
    }

    /// Writes a data file without the columns `field_ids`, as when files
    /// written outside Iceberg are added to a table (for example, Hive files
    /// that keep partition values only in their paths). Readers resolve the
    /// missing columns by Iceberg's column-projection rules.
    #[must_use]
    pub fn without_fields(mut self, field_ids: impl IntoIterator<Item = i32>) -> Self {
        self.omitted_fields.extend(field_ids);
        self
    }

    /// Limits Parquet row groups to `rows` rows each.
    #[must_use]
    pub fn row_groups_of(mut self, rows: usize) -> Self {
        self.row_group_rows = Some(rows);
        self
    }

    fn with_content(name: impl Into<String>, content: Content) -> Self {
        Self {
            name: name.into(),
            spec_id: None,
            partition: Vec::new(),
            content,
            row_group_rows: None,
            omitted_fields: Vec::new(),
        }
    }
}
