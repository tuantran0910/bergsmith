//! Immutable, hashed plans and their task artifacts.

// clippy's `allow-unwrap-in-tests` and `allow-panic-in-tests` cover only
// `#[test]` functions; the helpers fail the test on setup errors.
#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "test helpers fail the test"
)]

mod support;

use std::collections::{BTreeMap, BTreeSet};

use bergsmith_common::ids::{JobId, PlanId};
use bergsmith_fixtures::{Commit, FileSpec, Fixture, TableSpec, Value, row, spike};
use bergsmith_maintenance::{
    NoOpReason, PlanDocument, PlanError, PlanIds, PlanInput, PlanOutcome, PlannedDelete,
    PlannedFile, Planner, PlannerConfig, Rejection,
};
use bergsmith_protocol::artifact::DeleteContent;
use bergsmith_protocol::hashing::artifact_digest;
use futures::TryStreamExt;
use iceberg::TableIdent;
use iceberg::table::StaticTable;
use support::{ID, by_tag, data, tagged, with_properties};
use uuid::Uuid;

fn ids(n: u128) -> PlanIds {
    PlanIds {
        job_id: JobId::from_uuid(Uuid::from_u128(n)),
        plan_id: PlanId::from_uuid(Uuid::from_u128(n + 1)),
    }
}

async fn plan_with(fixture: &Fixture, config: PlannerConfig, ids: PlanIds) -> PlanOutcome {
    Planner::new(config)
        .unwrap()
        .plan(
            PlanInput {
                metadata: fixture.metadata(),
                metadata_location: Some(fixture.metadata_location()),
                file_io: fixture.file_io(),
                selection: &[],
            },
            ids,
        )
        .await
        .unwrap()
}

async fn planned(fixture: &Fixture) -> PlanDocument {
    match plan_with(fixture, PlannerConfig::default(), ids(1)).await {
        PlanOutcome::Planned(document) => *document,
        other => panic!("expected a plan, got {other:?}"),
    }
}

/// Partition `x` has five small files and `y` two; `z` has one with a
/// delete, committed after the data.
async fn mixed() -> Fixture {
    let mut commit = Commit::new();
    for index in 0..5 {
        commit = commit.file(data(&format!("x{index}"), 2).partition([Value::string("x")]));
    }
    commit = commit
        .file(data("y0", 2).partition([Value::string("y")]))
        .file(data("y1", 2).partition([Value::string("y")]))
        .file(data("z0", 2).partition([Value::string("z")]));
    TableSpec::new(tagged())
        .partitioned_by(by_tag())
        .commit(commit)
        .commit(
            Commit::new().file(
                FileSpec::equality_deletes("e", [ID], [row![ID => 0_i64]])
                    .partition([Value::string("z")]),
            ),
        )
        .generate()
        .await
        .unwrap()
}

#[tokio::test]
async fn admitted_bins_become_tasks_in_canonical_order() {
    let fixture = mixed().await;

    let document = planned(&fixture).await;

    let tasks: Vec<(u32, Vec<&str>, Vec<&str>)> = document
        .tasks()
        .iter()
        .map(|task| {
            let sources = task.sources().iter().map(PlannedFile::path).collect();
            let deletes = task.deletes().iter().map(PlannedDelete::path).collect();
            (task.ordinal(), sources, deletes)
        })
        .collect();
    let path = |name| fixture.path_of(name).unwrap();
    assert_eq!(
        tasks,
        [
            (0, ["x0", "x1", "x2", "x3", "x4"].map(path).to_vec(), vec![]),
            (1, vec![path("z0")], vec![path("e")]),
        ]
    );
    assert_eq!(document.task_ids().len(), 2);
    assert_eq!(document.baseline().snapshot_id(), 2);
    assert_eq!(document.estimate().tasks, 2);
}

#[tokio::test]
async fn artifacts_carry_the_baseline_files_and_output_sequence_number() {
    let fixture = mixed().await;
    let document = planned(&fixture).await;
    let task = &document.tasks()[1];

    let artifact = document.artifact_of(task).unwrap();

    assert_eq!(artifact_digest(&artifact), task.artifact_sha256().unwrap());
    assert_eq!(artifact.task_id, task.task_id().to_string());
    assert_eq!(artifact.plan_id, document.plan_id().to_string());
    let table = artifact.table.as_ref().unwrap();
    assert_eq!(table.table_uuid, fixture.metadata().uuid().to_string());
    assert_eq!(table.format_version, 2);
    let baseline = artifact.baseline.unwrap();
    assert_eq!((baseline.snapshot_id, baseline.sequence_number), (2, 2));
    assert_eq!(artifact.source_files.len(), 1);
    let source = &artifact.source_files[0];
    assert_eq!(source.data_sequence_number, 1);
    let partition = source.partition.as_ref().unwrap();
    assert_eq!(partition.spec_id, 0);
    assert_eq!(partition.values[0].value.as_deref(), Some(&b"z"[..]));
    let delete = &artifact.delete_files[0];
    assert_eq!(delete.content, i32::from(DeleteContent::Equality));
    assert_eq!(delete.equality_field_ids, [ID]);
    assert_eq!(delete.data_sequence_number, 2);
    let output = artifact.output.unwrap();
    assert_eq!(output.partition_spec_id, 0);
    assert_eq!(output.data_sequence_number, 2, "ADR 0004");
    assert_eq!(output.target_file_size_bytes, 256 * 1024 * 1024);
}

#[tokio::test]
async fn the_same_input_gives_the_same_plan_hash() {
    let fixture = mixed().await;

    let first = planned(&fixture).await;
    let second = planned(&fixture).await;
    let other = match plan_with(&fixture, PlannerConfig::default(), ids(10)).await {
        PlanOutcome::Planned(document) => document,
        other => panic!("{other:?}"),
    };

    assert_eq!(first, second);
    assert_eq!(first.sha256().unwrap(), second.sha256().unwrap());
    assert_ne!(first.sha256().unwrap(), other.sha256().unwrap());
    assert_ne!(first.task_ids(), other.task_ids());
}

#[tokio::test]
async fn a_persisted_plan_is_checked_against_its_hash() {
    let fixture = mixed().await;
    let document = planned(&fixture).await;
    let hash = document.sha256().unwrap().to_hex();
    // `serde_json::Value` sorts keys, as `jsonb` reorders them.
    let content = document.to_value().unwrap();

    let restored = PlanDocument::from_persisted(content.clone(), &hash).unwrap();
    assert_eq!(restored, document);

    let mut tampered = content.clone();
    tampered["tasks"][0]["sources"][0]["file_size_bytes"] = 1.into();
    let mut extended = content.clone();
    extended["extra"] = true.into();
    // A forger who recomputes the plan hash still cannot match the task's
    // artifact digest without recomputing it too.
    let forged = tampered.clone();
    let forged_hash = serde_json::from_value::<PlanDocument>(forged.clone())
        .unwrap()
        .sha256()
        .unwrap()
        .to_hex();
    let mut future = content.clone();
    future["version"] = 2.into();
    for (content, hash, expected) in [
        (tampered, hash.as_str(), "does not match its hash"),
        (extended, hash.as_str(), "does not decode"),
        (forged, forged_hash.as_str(), "artifact digest"),
        (future, hash.as_str(), "version 2"),
        (content, &"0".repeat(64), "does not match its hash"),
    ] {
        let error = PlanDocument::from_persisted(content, hash).unwrap_err();
        assert!(
            matches!(&error, PlanError::CorruptPlan(message) if message.contains(expected)),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn tables_without_work_end_as_noop() {
    let empty = TableSpec::new(tagged()).generate().await.unwrap();
    let few = TableSpec::new(tagged())
        .commit(Commit::new().file(data("a", 1)).file(data("b", 1)))
        .generate()
        .await
        .unwrap();

    assert_eq!(
        plan_with(&empty, PlannerConfig::default(), ids(1)).await,
        PlanOutcome::NoOp(NoOpReason::NoSnapshot)
    );
    assert_eq!(
        plan_with(&few, PlannerConfig::default(), ids(1)).await,
        PlanOutcome::NoOp(NoOpReason::NoBeneficialWork)
    );
    assert_eq!(NoOpReason::NoBeneficialWork.as_str(), "no_beneficial_work");
}

#[tokio::test]
async fn unsupported_tables_are_rejected() {
    let fixture = mixed().await;
    let metadata = with_properties(fixture.metadata(), &[("encryption.key-id", "k")]);

    let outcome = Planner::new(PlannerConfig::default())
        .unwrap()
        .plan(
            PlanInput {
                metadata: &metadata,
                metadata_location: Some(fixture.metadata_location()),
                file_io: fixture.file_io(),
                selection: &[],
            },
            ids(1),
        )
        .await
        .unwrap();

    assert_eq!(outcome, PlanOutcome::Rejected(vec![Rejection::Encrypted]));
}

/// Every file is a candidate and every bin is admitted.
fn everything() -> PlannerConfig {
    PlannerConfig {
        min_input_files: 1,
        min_files_removed: 0,
        ..PlannerConfig::default()
    }
}

/// The delete files iceberg-rust's scan planner attaches to each data file.
async fn scan_deletes(fixture: &Fixture) -> BTreeMap<String, BTreeSet<String>> {
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
    let mut deletes: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for task in tasks {
        deletes
            .entry(task.data_file_path.clone())
            .or_default()
            .extend(task.deletes.iter().map(|delete| delete.file_path.clone()));
    }
    deletes
}

#[tokio::test]
async fn spike_fixtures_plan_the_deletes_iceberg_rust_applies() {
    for spike in spike::all().unwrap() {
        let fixture = spike.generate().await.unwrap();
        let expected = scan_deletes(&fixture).await;

        let PlanOutcome::Planned(document) = plan_with(&fixture, everything(), ids(1)).await else {
            panic!("{} did not plan", spike.name);
        };

        let mut planned_files = BTreeSet::new();
        for task in document.tasks() {
            let mut wanted = BTreeSet::new();
            for source in task.sources() {
                assert!(
                    planned_files.insert(source.path().to_owned()),
                    "{}",
                    spike.name
                );
                wanted.extend(expected[source.path()].iter().cloned());
            }
            let planned: BTreeSet<String> = task
                .deletes()
                .iter()
                .map(|delete| delete.path().to_owned())
                .collect();
            assert_eq!(planned, wanted, "{}", spike.name);
        }
        assert_eq!(
            planned_files,
            expected.keys().cloned().collect(),
            "{}",
            spike.name
        );
    }
}
