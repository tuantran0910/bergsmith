//! The planner entry points.

use std::collections::BTreeMap;

use iceberg::io::FileIO;
use iceberg::spec::{Schema, TableMetadata};

use crate::baseline::PlanningBaseline;
use crate::binpack::{self, Bin, Item};
use crate::budget::{self, BinDisposition, BinSize, Estimate};
use crate::config::PlannerConfig;
use crate::deletes::DeleteIndex;
use crate::eligibility::{self, Candidate, Rejection, SkipReason};
use crate::error::PlanError;
use crate::inventory::Inventory;
use crate::partition::{PartitionKey, Specs};
use crate::plan::{PlanDocument, PlanIds, PlannedDelete, PlannedFile};
use crate::selection::{PartitionPredicate, Selection};

/// The table to plan, exactly as it was loaded.
#[derive(Clone, Copy, Debug)]
pub struct PlanInput<'a> {
    /// The loaded metadata.
    pub metadata: &'a TableMetadata,
    /// The metadata file it was read from; `None` for a staged table.
    pub metadata_location: Option<&'a str>,
    /// Reads the manifest list and manifests. The planner never reads data
    /// files.
    pub file_io: &'a FileIO,
    /// Which partitions to compact; empty selects the whole table.
    pub selection: &'a [PartitionPredicate],
}

/// A file count and their total size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileTally {
    /// Number of files.
    pub files: u64,
    /// Their total size in bytes.
    pub bytes: u64,
}

impl FileTally {
    fn add(&mut self, bytes: u64) {
        self.files += 1;
        self.bytes += bytes;
    }
}

/// An immutable assessment of a table state. It is not a job and cannot
/// publish.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Preview {
    /// The state assessed; `None` when the table has no `main` snapshot.
    pub baseline: Option<PlanningBaseline>,
    /// Why the table cannot be planned. Non-empty means no work is planned.
    pub rejections: Vec<Rejection>,
    /// Data files selected for compaction.
    pub candidates: FileTally,
    /// Data files left alone, by reason.
    pub skipped: BTreeMap<SkipReason, FileTally>,
    /// The candidates packed into bins, in canonical order.
    pub bins: Vec<BinPreview>,
    /// The admitted work.
    pub estimate: Estimate,
}

/// One bin of candidates: the input of one possible task.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BinPreview {
    /// The `(spec, partition)` group.
    pub partition: PartitionKey,
    /// Position among the group's bins, from 0.
    pub ordinal: usize,
    /// The data files in the bin.
    pub input: FileTally,
    /// Whether any of its files has an applicable delete.
    pub has_deletes: bool,
    /// Data files rewriting the bin is expected to remove.
    pub files_removed: u64,
    /// Whether the bin becomes a task of this job.
    pub disposition: BinDisposition,
}

/// What planning a job produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanOutcome {
    /// Work to execute; the job moves to `Queued` once it is persisted.
    Planned(Box<PlanDocument>),
    /// Nothing to do; the job ends as `NoOp`.
    NoOp(NoOpReason),
    /// The MVP cannot maintain the table; the job fails with these reasons.
    Rejected(Vec<Rejection>),
}

/// Why a job has nothing to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoOpReason {
    /// The table has no `main` snapshot.
    NoSnapshot,
    /// No bin is beneficial and within the job limits.
    NoBeneficialWork,
    /// Every remaining snapshot is younger than the retention cutoff or pinned.
    NoExpiredSnapshots,
    /// No listed object is old, unreferenced, and outside Bergsmith's prefix.
    NoOrphanFiles,
    /// No partition spec has two small data manifests to merge.
    NoManifestsToRewrite,
    /// Every live delete file applies to a live data file.
    NoDanglingDeletes,
}

impl NoOpReason {
    /// A stable name for the job's state reason.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoSnapshot => "no_snapshot",
            Self::NoBeneficialWork => "no_beneficial_work",
            Self::NoExpiredSnapshots => "no_expired_snapshots",
            Self::NoOrphanFiles => "no_orphan_files",
            Self::NoManifestsToRewrite => "no_manifests_to_rewrite",
            Self::NoDanglingDeletes => "no_dangling_deletes",
        }
    }
}

/// Plans maintenance of one table at a time.
#[derive(Clone, Debug)]
pub struct Planner {
    config: PlannerConfig,
}

impl Planner {
    /// A planner with `config`.
    ///
    /// # Errors
    ///
    /// [`PlanError::InvalidConfig`] when the limits are inconsistent.
    pub fn new(config: PlannerConfig) -> Result<Self, PlanError> {
        config.validate()?;
        Ok(Self { config })
    }

    /// The configuration plans are built with.
    #[must_use]
    pub const fn config(&self) -> &PlannerConfig {
        &self.config
    }

    /// Assesses `input` without creating any work.
    ///
    /// # Errors
    ///
    /// As for [`PlanningBaseline::capture`], [`PlanError::InvalidSelection`]
    /// for a predicate that fits no spec, and the metadata errors of
    /// [`PlanError`] when manifests cannot be read or are inconsistent.
    pub async fn preview(&self, input: PlanInput<'_>) -> Result<Preview, PlanError> {
        let assessment = self.assess(input).await?;
        let mut preview = Preview {
            baseline: assessment.baseline.clone(),
            rejections: assessment.rejections.clone(),
            candidates: FileTally::default(),
            skipped: BTreeMap::new(),
            bins: Vec::new(),
            estimate: Estimate::default(),
        };
        for (file, decision) in assessment.inventory.data.iter().zip(&assessment.decisions) {
            match decision {
                Ok(_) => preview.candidates.add(file.size),
                Err(reason) => preview.skipped.entry(*reason).or_default().add(file.size),
            }
        }
        let bins = self.bins(&assessment);
        let sizes = assessment.sizes(&bins);
        let (dispositions, estimate) = budget::admit(&sizes, &self.config);
        preview.estimate = estimate;
        preview.bins = bins
            .iter()
            .zip(&sizes)
            .zip(dispositions)
            .map(|((bin, size), disposition)| BinPreview {
                partition: bin.key.clone(),
                ordinal: bin.ordinal,
                input: FileTally {
                    files: size.files,
                    bytes: size.bytes,
                },
                has_deletes: size.has_deletes,
                files_removed: budget::files_removed(*size, &self.config),
                disposition,
            })
            .collect();
        Ok(preview)
    }

    /// The candidates of `assessment`, packed. Item identifiers are
    /// inventory data indices.
    fn bins<'a>(&self, assessment: &'a Assessment) -> Vec<Bin<'a>> {
        let items: Vec<Item<'a>> = assessment
            .decisions
            .iter()
            .filter_map(|decision| decision.as_ref().ok())
            .map(|candidate| {
                let file = &assessment.inventory.data[candidate.data];
                Item {
                    id: candidate.data,
                    key: &file.partition,
                    path: &file.path,
                    size: file.size,
                }
            })
            .collect();
        binpack::first_fit_decreasing(&items, self.config.max_group_input_bytes)
    }

    /// Builds the immutable plan of job `ids` for `input`.
    ///
    /// The plan holds the admitted bins as tasks in canonical order. The
    /// same `ids`, metadata, manifests, selection, and configuration always
    /// give the same plan and plan hash.
    ///
    /// # Errors
    ///
    /// As for [`preview`](Self::preview), and [`PlanError::CorruptPlan`] if
    /// a file size does not fit a task artifact.
    pub async fn plan(&self, input: PlanInput<'_>, ids: PlanIds) -> Result<PlanOutcome, PlanError> {
        let assessment = self.assess(input).await?;
        self.outcome(assessment, ids, input.selection)
    }

    /// The plan an assessment leads to.
    fn outcome(
        &self,
        assessment: Assessment,
        ids: PlanIds,
        selection: &[PartitionPredicate],
    ) -> Result<PlanOutcome, PlanError> {
        let Some(baseline) = assessment.baseline.clone() else {
            return Ok(PlanOutcome::NoOp(NoOpReason::NoSnapshot));
        };
        if !assessment.rejections.is_empty() {
            return Ok(PlanOutcome::Rejected(assessment.rejections));
        }
        let bins = self.bins(&assessment);
        let sizes = assessment.sizes(&bins);
        let (dispositions, estimate) = budget::admit(&sizes, &self.config);
        let tasks: Vec<_> = bins
            .iter()
            .zip(dispositions)
            .filter(|(_, disposition)| *disposition == BinDisposition::Admitted)
            .map(|(bin, _)| assessment.task(bin))
            .collect();
        if tasks.is_empty() {
            return Ok(PlanOutcome::NoOp(NoOpReason::NoBeneficialWork));
        }
        PlanDocument::assemble(
            ids,
            baseline,
            self.config.clone(),
            selection.to_vec(),
            estimate,
            tasks,
        )
        .map(|document| PlanOutcome::Planned(Box::new(document)))
    }

    async fn assess(&self, input: PlanInput<'_>) -> Result<Assessment, PlanError> {
        let metadata = input.metadata;
        let baseline = PlanningBaseline::capture(metadata, input.metadata_location)?;
        let specs = Specs::of(metadata)?;
        let selection = Selection::bind(input.selection, &specs)?;
        let mut assessment = Assessment {
            baseline,
            rejections: eligibility::table_rejections(metadata),
            inventory: Inventory::default(),
            decisions: Vec::new(),
        };
        let Some(baseline) = &assessment.baseline else {
            return Ok(assessment);
        };
        if !assessment.rejections.is_empty() {
            return Ok(assessment);
        }
        let inventory = Inventory::load(
            baseline,
            metadata,
            &specs,
            input.file_io,
            self.config.max_manifest_entries,
        )
        .await?;
        self.evaluate(
            &mut assessment,
            inventory,
            &specs,
            &selection,
            metadata.current_schema(),
        );
        Ok(assessment)
    }

    /// Checks the live files of an unrejected table and classifies them.
    fn evaluate(
        &self,
        assessment: &mut Assessment,
        inventory: Inventory,
        specs: &Specs,
        selection: &Selection,
        schema: &Schema,
    ) {
        assessment.rejections = eligibility::file_rejections(&inventory, schema);
        if assessment.rejections.is_empty() {
            let index = DeleteIndex::new(&inventory.deletes, specs);
            assessment.rejections = eligibility::scope_rejections(&inventory, &index);
            if assessment.rejections.is_empty() {
                assessment.decisions =
                    eligibility::classify(&inventory, &index, selection, &self.config);
            }
        }
        assessment.inventory = inventory;
    }
}

impl Assessment {
    /// The size of each bin.
    fn sizes(&self, bins: &[Bin<'_>]) -> Vec<BinSize> {
        bins.iter()
            .map(|bin| BinSize {
                files: bin.items.len() as u64,
                bytes: bin.bytes,
                has_deletes: bin.items.iter().any(|&data| self.has_deletes(data)),
            })
            .collect()
    }

    /// The sources and deletes of the task `bin` becomes.
    fn task(&self, bin: &Bin<'_>) -> (PartitionKey, Vec<PlannedFile>, Vec<PlannedDelete>) {
        let sources = bin
            .items
            .iter()
            .map(|&data| PlannedFile::of(&self.inventory.data[data]))
            .collect();
        let mut delete_indices: Vec<usize> = bin
            .items
            .iter()
            .filter_map(|&data| self.decisions.get(data)?.as_ref().ok())
            .flat_map(|candidate| candidate.deletes.iter().copied())
            .collect();
        delete_indices.sort_unstable();
        delete_indices.dedup();
        let deletes = delete_indices
            .into_iter()
            .map(|index| PlannedDelete::of(&self.inventory.deletes[index]))
            .collect();
        (bin.key.clone(), sources, deletes)
    }

    /// Whether the data file at `data` is a candidate with deletes.
    fn has_deletes(&self, data: usize) -> bool {
        self.decisions
            .get(data)
            .and_then(|decision| decision.as_ref().ok())
            .is_some_and(|candidate| !candidate.deletes.is_empty())
    }
}

/// Everything planning learns about a table before grouping.
struct Assessment {
    baseline: Option<PlanningBaseline>,
    rejections: Vec<Rejection>,
    inventory: Inventory,
    /// Per data file, in inventory order. Empty when rejected.
    decisions: Vec<Result<Candidate, SkipReason>>,
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use bergsmith_common::ids::{JobId, PlanId};
    use iceberg::spec::{
        DataFileFormat, FormatVersion, Literal, NestedField, PrimitiveType, SortOrder, Struct,
        TableMetadataBuilder, Transform, Type, UnboundPartitionSpec,
    };
    use proptest::prelude::*;
    use uuid::Uuid;

    use super::*;
    use crate::inventory::{DeleteEntry, DeleteKind, FileEntry};

    const TAGS: [Option<&str>; 3] = [Some("a"), Some("b"), None];

    /// Spec 0 is identity on `tag`; spec 1 is unpartitioned.
    fn metadata() -> TableMetadata {
        let schema = Schema::builder()
            .with_fields([
                Arc::new(NestedField::required(
                    1,
                    "id",
                    Type::Primitive(PrimitiveType::Long),
                )),
                Arc::new(NestedField::optional(
                    2,
                    "tag",
                    Type::Primitive(PrimitiveType::String),
                )),
            ])
            .build()
            .unwrap();
        let by_tag = UnboundPartitionSpec::builder()
            .add_partition_field(2, "tag", Transform::Identity)
            .unwrap()
            .build();
        TableMetadataBuilder::new(
            schema,
            by_tag,
            SortOrder::unsorted_order(),
            "file:///t".into(),
            FormatVersion::V2,
            std::collections::HashMap::new(),
        )
        .unwrap()
        .add_partition_spec(UnboundPartitionSpec::builder().build())
        .unwrap()
        .build()
        .unwrap()
        .metadata
    }

    fn config() -> PlannerConfig {
        PlannerConfig {
            target_file_size_bytes: 1_000,
            min_file_size_bytes: 750,
            max_group_input_bytes: 3_000,
            max_manifest_entries: 1_000,
            max_job_input_bytes: 20_000,
            max_job_files: 40,
            max_concurrent_groups: 2,
            min_input_files: 2,
            min_files_removed: 1,
            throughput_bytes_per_second: 1_000,
            task_overhead_millis: 10,
            duration_budget_millis: 20_000,
        }
    }

    /// A partition: 0..3 picks a tag of spec 0, 3 picks spec 1.
    fn key(specs: &Specs, choice: usize) -> PartitionKey {
        if choice == TAGS.len() {
            return specs.key(1, &Struct::from_iter([])).unwrap();
        }
        let literal = TAGS[choice].map(Literal::string);
        specs.key(0, &Struct::from_iter([literal])).unwrap()
    }

    #[derive(Clone, Debug)]
    struct Table {
        data: Vec<(usize, u64, i64)>,
        deletes: Vec<(usize, i64, bool)>,
        seed: u64,
    }

    fn table() -> impl Strategy<Value = Table> {
        (
            // Coarse sizes make ties common, so tie-breaking is exercised.
            prop::collection::vec(
                (0..=TAGS.len(), (1_u64..10).prop_map(|n| n * 250), 1_i64..5),
                0..60,
            ),
            prop::collection::vec((0..=TAGS.len(), 1_i64..6, any::<bool>()), 0..6),
            any::<u64>(),
        )
            .prop_map(|(data, deletes, seed)| Table {
                data,
                deletes,
                seed,
            })
    }

    fn entries(table: &Table, specs: &Specs) -> (Vec<FileEntry>, Vec<DeleteEntry>) {
        let file = |path: String, partition, size, sequence_number| FileEntry {
            path,
            format: DataFileFormat::Parquet,
            size,
            records: size / 10,
            sequence_number,
            partition,
            key_metrics: BTreeMap::new(),
        };
        let data = table
            .data
            .iter()
            .enumerate()
            .map(|(index, &(choice, size, seq))| {
                file(
                    format!("file:///t/data/d{index:03}"),
                    key(specs, choice),
                    size,
                    seq,
                )
            })
            .collect();
        let deletes = table
            .deletes
            .iter()
            .enumerate()
            .map(|(index, &(choice, seq, equality))| DeleteEntry {
                file: file(
                    format!("file:///t/data/x{index:03}"),
                    key(specs, choice),
                    50,
                    seq,
                ),
                kind: if equality {
                    DeleteKind::Equality { field_ids: vec![1] }
                } else {
                    DeleteKind::Position {
                        referenced: None,
                        path_bounds: None,
                    }
                },
            })
            .collect();
        (data, deletes)
    }

    /// A deterministic shuffle, standing in for manifest order.
    fn shuffle<T>(items: &mut [T], mut seed: u64) {
        for index in (1..items.len()).rev() {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let other = usize::try_from(seed >> 33).unwrap() % (index + 1);
            items.swap(index, other);
        }
    }

    fn plan_of(
        mut data: Vec<FileEntry>,
        mut deletes: Vec<DeleteEntry>,
        seed: u64,
    ) -> (PlanOutcome, Inventory) {
        let metadata = metadata();
        let specs = Specs::of(&metadata).unwrap();
        shuffle(&mut data, seed);
        shuffle(&mut deletes, seed.rotate_left(17));
        let inventory = Inventory::from_entries(data, deletes).unwrap();
        let planner = Planner::new(config()).unwrap();
        let mut assessment = Assessment {
            baseline: Some(PlanningBaseline::for_tests()),
            rejections: Vec::new(),
            inventory: Inventory::default(),
            decisions: Vec::new(),
        };
        let selection = Selection::bind(&[], &specs).unwrap();
        planner.evaluate(
            &mut assessment,
            inventory.clone(),
            &specs,
            &selection,
            metadata.current_schema(),
        );
        let ids = PlanIds {
            job_id: JobId::from_uuid(Uuid::from_u128(7)),
            plan_id: PlanId::from_uuid(Uuid::from_u128(8)),
        };
        (planner.outcome(assessment, ids, &[]).unwrap(), inventory)
    }

    proptest! {
        #[test]
        fn manifest_order_never_changes_the_plan_hash(table in table(), other_seed in any::<u64>()) {
            let specs = Specs::of(&metadata()).unwrap();
            let (data, deletes) = entries(&table, &specs);

            let (first, _) = plan_of(data.clone(), deletes.clone(), table.seed);
            let (second, _) = plan_of(data, deletes, other_seed);

            prop_assert_eq!(&first, &second);
            if let PlanOutcome::Planned(document) = &first {
                let PlanOutcome::Planned(other) = &second else { unreachable!() };
                prop_assert_eq!(document.sha256().unwrap(), other.sha256().unwrap());
            }
        }

        #[test]
        fn plans_stay_within_every_limit(table in table()) {
            let specs = Specs::of(&metadata()).unwrap();
            let (data, deletes) = entries(&table, &specs);
            let config = config();

            let (outcome, inventory) = plan_of(data, deletes, table.seed);

            let PlanOutcome::Planned(document) = outcome else {
                return Ok(());
            };
            let by_path: BTreeMap<&str, &FileEntry> =
                inventory.data.iter().map(|file| (file.path.as_str(), file)).collect();
            let index = DeleteIndex::new(&inventory.deletes, &specs);
            let mut seen = BTreeSet::new();
            let (mut files, mut bytes) = (0, 0);
            for task in document.tasks() {
                let task_bytes: u64 = task.sources().iter().map(PlannedFile::file_size_bytes).sum();
                prop_assert!(task_bytes <= config.max_group_input_bytes);
                let mut expected_deletes = BTreeSet::new();
                for source in task.sources() {
                    prop_assert!(seen.insert(source.path().to_owned()), "a file is in two tasks");
                    let file = by_path[source.path()];
                    prop_assert_eq!(&file.partition, task.partition());
                    let applicable = index.applicable(file);
                    prop_assert!(file.size < config.min_file_size_bytes || !applicable.is_empty());
                    expected_deletes.extend(applicable.iter().map(|delete| delete.file.path.clone()));
                }
                let planned: BTreeSet<String> =
                    task.deletes().iter().map(|delete| delete.path().to_owned()).collect();
                prop_assert_eq!(planned, expected_deletes);
                files += task.sources().len() as u64;
                bytes += task_bytes;
            }
            let estimate = document.estimate();
            prop_assert_eq!((estimate.files, estimate.bytes), (files, bytes));
            prop_assert!(files <= config.max_job_files);
            prop_assert!(bytes <= config.max_job_input_bytes);
            prop_assert!(estimate.duration_millis <= config.duration_budget_millis);
            prop_assert_eq!(estimate.tasks, document.tasks().len() as u64);
        }
    }
}
