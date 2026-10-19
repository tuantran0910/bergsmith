//! Writing a declared table as real Iceberg v2 metadata and Parquet files.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use iceberg::io::FileIO;
use iceberg::spec::{
    DataContentType, DataFile, DataFileBuilder, DataFileFormat, Datum, FormatVersion, MAIN_BRANCH,
    ManifestFile, ManifestListWriter, ManifestWriterBuilder, Operation, Schema, SchemaRef,
    Snapshot, SortOrder, Summary, TableMetadata, TableMetadataBuilder, UNASSIGNED_SEQUENCE_NUMBER,
};
use tempfile::TempDir;
use uuid::Uuid;

use crate::declare::{Commit, Content, FileSpec, TableSpec};
use crate::encode;
use crate::error::{FixtureError, Result};
use crate::metrics::ColumnMetrics;

/// The table UUID of every fixture.
const TABLE_UUID: Uuid = Uuid::from_u128(0x0199_0000_0000_7000_8000_0000_0000_0b5e);
/// The first commit's timestamp: 2026-01-01T00:00:00Z.
const FIRST_COMMIT_MS: i64 = 1_767_225_600_000;
/// The interval between commit timestamps.
const COMMIT_INTERVAL_MS: i64 = 60_000;

/// A generated table on the local filesystem.
#[derive(Debug)]
pub struct Fixture {
    /// Keeps a temporary directory alive for as long as the fixture.
    _dir: Option<TempDir>,
    root: PathBuf,
    file_io: FileIO,
    metadata_location: String,
    metadata: TableMetadata,
    files: BTreeMap<String, String>,
}

impl Fixture {
    /// The table's current metadata.
    #[must_use]
    pub fn metadata(&self) -> &TableMetadata {
        &self.metadata
    }

    /// A `FileIO` that reads the table's files.
    #[must_use]
    pub fn file_io(&self) -> &FileIO {
        &self.file_io
    }

    /// Where the current `metadata.json` is.
    #[must_use]
    pub fn metadata_location(&self) -> &str {
        &self.metadata_location
    }

    /// The local directory that holds the table.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The full path of the fixture file declared as `name`.
    ///
    /// # Errors
    ///
    /// [`FixtureError::Declaration`] if the fixture declares no such file.
    pub fn path_of(&self, name: &str) -> Result<&str> {
        self.files
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| FixtureError::Declaration(format!("no fixture file named {name}")))
    }
}

impl TableSpec {
    /// Writes the table to a new temporary directory, removed when the
    /// fixture is dropped.
    ///
    /// # Errors
    ///
    /// As for [`TableSpec::write_to`].
    pub async fn generate(&self) -> Result<Fixture> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().to_owned();
        self.write(&path, Some(dir)).await
    }

    /// Writes the table under `dir`, which must exist. The same declaration
    /// and directory always produce the same paths and Parquet bytes.
    ///
    /// # Errors
    ///
    /// [`FixtureError::Declaration`] if the declaration is inconsistent, and
    /// the other variants if building or writing metadata or files fails.
    pub async fn write_to(&self, dir: &Path) -> Result<Fixture> {
        self.write(dir, None).await
    }

    async fn write(&self, dir: &Path, owned: Option<TempDir>) -> Result<Fixture> {
        let root = dir.canonicalize()?;
        let location = format!("file://{}", root.display());
        let plan = self.plan(&location)?;
        let file_io = FileIO::new_with_fs();
        let mut writer = Writer {
            file_io: &file_io,
            location: &location,
            metadata: &plan.metadata,
            column_metrics: self.column_metrics,
            files: BTreeMap::new(),
            manifests: Vec::new(),
        };
        for (commit, state) in self.commits.iter().zip(&plan.states) {
            writer.write_commit(commit, state).await?;
        }
        let metadata_location = format!("{location}/metadata/v{}.metadata.json", plan.states.len());
        write(
            &file_io,
            &metadata_location,
            serde_json::to_vec(&plan.metadata)?,
        )
        .await?;
        Ok(Fixture {
            _dir: owned,
            root,
            files: writer.files,
            file_io,
            metadata_location,
            metadata: plan.metadata,
        })
    }

    /// Builds the final metadata, every snapshot included, before any file
    /// is written. Snapshots only reference their manifest lists by path.
    fn plan(&self, location: &str) -> Result<Plan> {
        let mut builder = TableMetadataBuilder::new(
            self.schema.clone(),
            self.spec.clone(),
            SortOrder::unsorted_order(),
            location.to_owned(),
            FormatVersion::V2,
            HashMap::new(),
        )?
        .assign_uuid(TABLE_UUID);
        let (mut schema_id, mut spec_id) = (0, 0);
        let mut states = Vec::with_capacity(self.commits.len());
        let mut parent = None;
        for (index, commit) in (0_i64..).zip(&self.commits) {
            if let Some(schema) = &commit.schema {
                builder = builder.add_current_schema(schema.clone())?;
                schema_id += 1;
            }
            if let Some(spec) = &commit.spec {
                builder = builder.add_default_partition_spec(spec.clone())?;
                spec_id += 1;
            }
            let state = CommitState {
                snapshot_id: index + 1,
                parent,
                sequence_number: index + 1,
                schema_id,
                spec_id,
            };
            let snapshot = Snapshot::builder()
                .with_snapshot_id(state.snapshot_id)
                .with_parent_snapshot_id(parent)
                .with_sequence_number(state.sequence_number)
                .with_timestamp_ms(FIRST_COMMIT_MS + index * COMMIT_INTERVAL_MS)
                .with_manifest_list(manifest_list_path(location, state.snapshot_id))
                .with_summary(Summary {
                    operation: operation(commit),
                    additional_properties: HashMap::new(),
                })
                .with_schema_id(schema_id)
                .build();
            builder = builder.set_branch_snapshot(snapshot, MAIN_BRANCH)?;
            parent = Some(state.snapshot_id);
            states.push(state);
        }
        let metadata = builder.build()?.metadata;
        self.check_ids(&metadata, schema_id, spec_id)?;
        Ok(Plan { metadata, states })
    }

    /// Checks that the builder kept the declared field IDs and assigned
    /// schema and spec IDs in declaration order.
    fn check_ids(&self, metadata: &TableMetadata, schema_id: i32, spec_id: i32) -> Result<()> {
        let initial = metadata
            .schema_by_id(0)
            .is_some_and(|schema| schema.as_struct() == self.schema.as_struct());
        if !initial {
            return Err(FixtureError::Declaration(
                "the initial schema must number its fields from 1 in declaration order".into(),
            ));
        }
        if metadata.current_schema_id() != schema_id
            || metadata.default_partition_spec_id() != spec_id
        {
            return Err(FixtureError::Declaration(
                "every evolved schema and spec must differ from the earlier ones".into(),
            ));
        }
        Ok(())
    }
}

/// The final metadata and each commit's resolved IDs.
struct Plan {
    metadata: TableMetadata,
    states: Vec<CommitState>,
}

#[derive(Clone, Copy)]
struct CommitState {
    snapshot_id: i64,
    parent: Option<i64>,
    sequence_number: i64,
    schema_id: i32,
    spec_id: i32,
}

struct Writer<'a> {
    file_io: &'a FileIO,
    location: &'a str,
    metadata: &'a TableMetadata,
    /// Whether data and equality-delete entries record column metrics.
    column_metrics: bool,
    files: BTreeMap<String, String>,
    /// The manifests of the latest snapshot, with sequence numbers assigned.
    manifests: Vec<ManifestFile>,
}

impl Writer<'_> {
    async fn write_commit(&mut self, commit: &Commit, state: &CommitState) -> Result<()> {
        let mut groups: BTreeMap<(bool, i32), Vec<DataFile>> = BTreeMap::new();
        for file in &commit.files {
            let data_file = self.write_file(file, state).await?;
            let is_delete = data_file.content_type() != DataContentType::Data;
            groups
                .entry((is_delete, data_file.partition_spec_id()))
                .or_default()
                .push(data_file);
        }
        let mut manifests = Vec::with_capacity(groups.len() + self.manifests.len());
        for (index, ((is_delete, spec_id), files)) in groups.into_iter().enumerate() {
            let path = format!(
                "{}/metadata/{}-m{index}.avro",
                self.location, state.snapshot_id
            );
            let spec = self
                .metadata
                .partition_spec_by_id(spec_id)
                .ok_or_else(|| unknown_spec(spec_id))?;
            let builder = ManifestWriterBuilder::new(
                self.file_io.new_output(&path)?,
                Some(state.snapshot_id),
                self.schema(state.schema_id)?,
                spec.as_ref().clone(),
            );
            let mut writer = if is_delete {
                builder.build_v2_deletes()
            } else {
                builder.build_v2_data()
            };
            for file in files {
                writer.add_file(file, UNASSIGNED_SEQUENCE_NUMBER)?;
            }
            let mut manifest = writer.write_manifest_file().await?;
            manifest.sequence_number = state.sequence_number;
            manifest.min_sequence_number = state.sequence_number;
            manifests.push(manifest);
        }
        manifests.append(&mut self.manifests);
        let output = self
            .file_io
            .new_output(manifest_list_path(self.location, state.snapshot_id))?;
        let mut list = ManifestListWriter::v2(
            output.writer().await?,
            state.snapshot_id,
            state.parent,
            state.sequence_number,
        );
        list.add_manifests(manifests.iter().cloned())?;
        list.close().await?;
        self.manifests = manifests;
        Ok(())
    }

    async fn write_file(&mut self, file: &FileSpec, state: &CommitState) -> Result<DataFile> {
        let path = format!("{}/data/{}.parquet", self.location, file.name);
        let schema = self.schema(state.schema_id)?;
        let spec_id = file.spec_id.unwrap_or(state.spec_id);
        let fields = self
            .metadata
            .partition_spec_by_id(spec_id)
            .ok_or_else(|| unknown_spec(spec_id))?
            .fields()
            .len();
        if file.partition.len() != fields {
            return Err(FixtureError::Declaration(format!(
                "file {} has {} partition values for a spec with {fields} fields",
                file.name,
                file.partition.len()
            )));
        }
        if !file.omitted_fields.is_empty() && !matches!(file.content, Content::Data(_)) {
            return Err(FixtureError::Declaration(format!(
                "only data files can omit fields, but {} is a delete file",
                file.name
            )));
        }
        let mut path_bounds = None;
        let mut metrics = ColumnMetrics::default();
        let (content, batch, equality_ids) = match &file.content {
            Content::Data(rows) => {
                let written = without(&schema, &file.omitted_fields)?;
                if self.column_metrics {
                    metrics = ColumnMetrics::of(&written, rows);
                }
                (
                    DataContentType::Data,
                    encode::rows_batch(&written, rows)?,
                    None,
                )
            }
            Content::PositionDeletes(deletes) => {
                let resolved: Vec<(String, i64)> = deletes
                    .iter()
                    .map(|(name, pos)| Ok((self.data_path(name)?.to_owned(), *pos)))
                    .collect::<Result<_>>()?;
                // Writers record full `file_path` bounds, so readers can
                // tell which data files a position delete may touch.
                path_bounds = resolved
                    .iter()
                    .map(|(path, _)| path)
                    .min()
                    .zip(resolved.iter().map(|(path, _)| path).max())
                    .map(|(lower, upper)| (lower.clone(), upper.clone()));
                (
                    DataContentType::PositionDeletes,
                    encode::position_deletes_batch(resolved)?,
                    None,
                )
            }
            Content::EqualityDeletes { field_ids, keys } => {
                let projected = project(&schema, field_ids)?;
                if self.column_metrics {
                    metrics = ColumnMetrics::of(&projected, keys);
                }
                (
                    DataContentType::EqualityDeletes,
                    encode::rows_batch(&projected, keys)?,
                    Some(field_ids.clone()),
                )
            }
        };
        let bound = |path: Option<String>, mut bounds: HashMap<i32, Datum>| {
            bounds.extend(path.map(|path| (encode::FILE_PATH_FIELD_ID, Datum::string(path))));
            bounds
        };
        let (lower, upper) = path_bounds.unzip();
        let bytes = encode::parquet_bytes(&batch, file.row_group_rows)?;
        let size = bytes.len() as u64;
        write(self.file_io, &path, bytes).await?;
        if self.files.insert(file.name.clone(), path.clone()).is_some() {
            return Err(FixtureError::Declaration(format!(
                "file name {} is declared twice",
                file.name
            )));
        }
        DataFileBuilder::default()
            .content(content)
            .file_path(path)
            .file_format(DataFileFormat::Parquet)
            .partition(encode::partition(&file.partition))
            .partition_spec_id(spec_id)
            .record_count(batch.num_rows() as u64)
            .file_size_in_bytes(size)
            .equality_ids(equality_ids)
            .value_counts(metrics.value_counts)
            .null_value_counts(metrics.null_value_counts)
            .nan_value_counts(metrics.nan_value_counts)
            .lower_bounds(bound(lower, metrics.lower_bounds))
            .upper_bounds(bound(upper, metrics.upper_bounds))
            .build()
            .map_err(|error| FixtureError::Declaration(error.to_string()))
    }

    fn data_path(&self, name: &str) -> Result<&str> {
        self.files.get(name).map(String::as_str).ok_or_else(|| {
            FixtureError::Declaration(format!(
                "position delete names {name}, which is not an earlier data file"
            ))
        })
    }

    fn schema(&self, schema_id: i32) -> Result<SchemaRef> {
        self.metadata
            .schema_by_id(schema_id)
            .cloned()
            .ok_or_else(|| FixtureError::Content(format!("schema {schema_id} is missing")))
    }
}

/// `schema` restricted to `field_ids`, in schema order. The delete file's
/// `equality_ids` keep the declared order.
fn project(schema: &Schema, field_ids: &[i32]) -> Result<Schema> {
    let wanted: BTreeSet<i32> = field_ids.iter().copied().collect();
    let fields: Vec<_> = schema
        .as_struct()
        .fields()
        .iter()
        .filter(|field| wanted.contains(&field.id))
        .cloned()
        .collect();
    if fields.len() != wanted.len() {
        return Err(FixtureError::Declaration(format!(
            "equality fields {field_ids:?} are not all in the current schema"
        )));
    }
    Ok(Schema::builder().with_fields(fields).build()?)
}

/// `schema` without `omitted` fields. Every omitted field must exist.
fn without(schema: &Schema, omitted: &[i32]) -> Result<Schema> {
    if omitted.is_empty() {
        return Ok(schema.clone());
    }
    if let Some(id) = omitted.iter().find(|id| schema.field_by_id(**id).is_none()) {
        return Err(FixtureError::Declaration(format!(
            "omitted field {id} is not in the file's schema"
        )));
    }
    let fields: Vec<_> = schema
        .as_struct()
        .fields()
        .iter()
        .filter(|field| !omitted.contains(&field.id))
        .cloned()
        .collect();
    Ok(Schema::builder().with_fields(fields).build()?)
}

fn operation(commit: &Commit) -> Operation {
    let data = commit
        .files
        .iter()
        .any(|file| matches!(file.content, Content::Data(_)));
    let deletes = commit
        .files
        .iter()
        .any(|file| !matches!(file.content, Content::Data(_)));
    match (data, deletes) {
        (true, true) => Operation::Overwrite,
        (false, true) => Operation::Delete,
        _ => Operation::Append,
    }
}

fn manifest_list_path(location: &str, snapshot_id: i64) -> String {
    format!("{location}/metadata/snap-{snapshot_id}.avro")
}

fn unknown_spec(spec_id: i32) -> FixtureError {
    FixtureError::Declaration(format!("partition spec {spec_id} is not declared"))
}

async fn write(file_io: &FileIO, path: &str, bytes: Vec<u8>) -> Result<()> {
    file_io.new_output(path)?.write(Bytes::from(bytes)).await?;
    Ok(())
}
