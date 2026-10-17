//! The planning baseline pins the table state a plan is built from.

// clippy's `allow-unwrap-in-tests` covers only `#[test]` functions; the
// helpers fail the test on setup errors.
#![expect(clippy::unwrap_used, reason = "test helpers fail the test")]

mod support;

use std::collections::BTreeMap;

use bergsmith_fixtures::{Commit, TableSpec};
use bergsmith_maintenance::{PlanError, PlanningBaseline};
use iceberg::spec::UnboundPartitionSpec;
use support::{by_tag, data, tagged, with_properties};

async fn two_commits() -> bergsmith_fixtures::Fixture {
    TableSpec::new(tagged())
        .partitioned_by(by_tag())
        .commit(
            Commit::new().file(data("x", 2).partition([bergsmith_fixtures::Value::string("x")])),
        )
        .commit(
            Commit::new()
                .evolve_spec(UnboundPartitionSpec::builder().build())
                .file(data("plain", 2)),
        )
        .generate()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_baseline_pins_the_main_snapshot_and_table_state() {
    let fixture = two_commits().await;
    let metadata = fixture.metadata();

    let baseline = PlanningBaseline::capture(metadata, Some(fixture.metadata_location()))
        .unwrap()
        .unwrap();

    assert_eq!(baseline.table_uuid(), metadata.uuid());
    assert_eq!(baseline.metadata_location(), fixture.metadata_location());
    assert_eq!(baseline.format_version(), 2);
    assert_eq!(baseline.snapshot_id(), 2);
    assert_eq!(baseline.sequence_number(), 2);
    assert_eq!(baseline.schema_id(), metadata.current_schema_id());
    assert_eq!(baseline.last_column_id(), 2);
    assert_eq!(baseline.default_spec_id(), 1);
    assert_eq!(baseline.last_partition_id(), metadata.last_partition_id());
    assert_eq!(baseline.default_sort_order_id(), 0);
    assert_eq!(baseline.location(), metadata.location());
    assert_eq!(
        baseline.data_location(),
        format!("{}/data", metadata.location())
    );
    assert!(baseline.properties().is_empty());
}

#[tokio::test]
async fn the_data_location_follows_write_data_path() {
    let fixture = two_commits().await;
    let elsewhere = format!("{}/elsewhere/", fixture.metadata().location());
    let metadata = with_properties(fixture.metadata(), &[("write.data.path", &elsewhere)]);

    let baseline = PlanningBaseline::capture(&metadata, Some("file:///m.json"))
        .unwrap()
        .unwrap();

    assert_eq!(
        baseline.data_location(),
        elsewhere.trim_end_matches('/'),
        "the location is canonical"
    );
}

#[tokio::test]
async fn only_plan_relevant_properties_are_recorded() {
    let fixture = two_commits().await;
    let metadata = with_properties(
        fixture.metadata(),
        &[
            ("write.format.default", "parquet"),
            ("write.metadata.path", "file:///meta"),
            ("commit.retry.num-retries", "9"),
        ],
    );

    let baseline = PlanningBaseline::capture(&metadata, Some("file:///m.json"))
        .unwrap()
        .unwrap();

    assert_eq!(
        baseline.properties(),
        &BTreeMap::from([
            ("write.format.default".to_owned(), "parquet".to_owned()),
            ("write.metadata.path".to_owned(), "file:///meta".to_owned()),
        ])
    );
}

#[tokio::test]
async fn a_table_without_a_main_snapshot_has_no_baseline() {
    let fixture = TableSpec::new(tagged()).generate().await.unwrap();

    let baseline =
        PlanningBaseline::capture(fixture.metadata(), Some(fixture.metadata_location())).unwrap();

    assert_eq!(baseline, None);
}

#[tokio::test]
async fn a_staged_table_is_refused() {
    let fixture = two_commits().await;

    let error = PlanningBaseline::capture(fixture.metadata(), None).unwrap_err();

    assert!(
        matches!(error, PlanError::StagedTable { table_uuid } if table_uuid == fixture.metadata().uuid())
    );
}

#[tokio::test]
async fn an_invalid_data_location_is_refused() {
    let fixture = two_commits().await;
    let metadata = with_properties(fixture.metadata(), &[("write.data.path", "data/relative")]);

    let error = PlanningBaseline::capture(&metadata, Some("file:///m.json")).unwrap_err();

    assert!(matches!(error, PlanError::Location(_)), "{error:?}");
}

#[tokio::test]
async fn the_baseline_round_trips_through_json() {
    let fixture = two_commits().await;
    let baseline = PlanningBaseline::capture(fixture.metadata(), Some(fixture.metadata_location()))
        .unwrap()
        .unwrap();

    let json = serde_json::to_value(&baseline).unwrap();
    let mut extended = json.clone();
    extended["unexpected"] = serde_json::Value::Bool(true);

    assert_eq!(
        serde_json::from_value::<PlanningBaseline>(json).unwrap(),
        baseline
    );
    assert!(serde_json::from_value::<PlanningBaseline>(extended).is_err());
}
