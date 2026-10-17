//! The planner entry points.

use std::collections::BTreeMap;

use iceberg::io::FileIO;
use iceberg::spec::TableMetadata;

use crate::baseline::PlanningBaseline;
use crate::binpack::{self, Bin, Item};
use crate::config::PlannerConfig;
use crate::deletes::DeleteIndex;
use crate::eligibility::{self, Candidate, Rejection, SkipReason};
use crate::error::PlanError;
use crate::inventory::Inventory;
use crate::partition::{PartitionKey, Specs};
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
        };
        for (file, decision) in assessment.inventory.data.iter().zip(&assessment.decisions) {
            match decision {
                Ok(_) => preview.candidates.add(file.size),
                Err(reason) => preview.skipped.entry(*reason).or_default().add(file.size),
            }
        }
        preview.bins = self
            .bins(&assessment)
            .into_iter()
            .map(|bin| BinPreview {
                partition: bin.key.clone(),
                ordinal: bin.ordinal,
                input: FileTally {
                    files: bin.items.len() as u64,
                    bytes: bin.bytes,
                },
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
        assessment.rejections = eligibility::file_rejections(&inventory, metadata.current_schema());
        if assessment.rejections.is_empty() {
            let index = DeleteIndex::new(&inventory.deletes, &specs);
            assessment.decisions =
                eligibility::classify(&inventory, &index, &selection, &self.config);
        }
        assessment.inventory = inventory;
        Ok(assessment)
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
