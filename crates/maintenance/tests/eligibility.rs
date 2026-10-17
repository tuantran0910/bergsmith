//! Table checks, preview rejection reasons, and file eligibility.

// clippy's `allow-unwrap-in-tests` covers only `#[test]` functions; the
// helpers fail the test on setup errors.
#![expect(clippy::unwrap_used, reason = "test helpers fail the test")]

mod support;

use std::collections::{BTreeMap, HashMap};

use bergsmith_fixtures::{Commit, FileSpec, Fixture, TableSpec, Value, row};
use bergsmith_maintenance::{
    FileTally, KeyProblem, PartitionPredicate, PlanError, PlannerConfig, Preview, Rejection,
    SkipReason,
};
use iceberg::spec::{
    FormatVersion, NestedField, NullOrder, PartitionSpec, PrimitiveType, SortDirection, SortField,
    SortOrder, TableMetadata, TableMetadataBuilder, Transform, Type,
};
use serde_json::json;
use support::{ID, TAG, by_tag, data, deletes_only, preview_with, schema, tagged, with_properties};

async fn preview(fixture: &Fixture, config: PlannerConfig) -> Preview {
    preview_with(fixture, fixture.metadata(), config, &[])
        .await
        .unwrap()
}

async fn selected(fixture: &Fixture, selection: &[PartitionPredicate]) -> Preview {
    preview_with(
        fixture,
        fixture.metadata(),
        PlannerConfig::default(),
        selection,
    )
    .await
    .unwrap()
}

fn size_of(fixture: &Fixture, name: &str) -> u64 {
    let path = fixture.path_of(name).unwrap();
    std::fs::metadata(path.strip_prefix("file://").unwrap())
        .unwrap()
        .len()
}

fn tally(fixture: &Fixture, names: &[&str]) -> FileTally {
    FileTally {
        files: names.len() as u64,
        bytes: names.iter().map(|name| size_of(fixture, name)).sum(),
    }
}

/// Data files `x` and `y` in identity partitions on `tag`.
async fn partitioned() -> Fixture {
    TableSpec::new(tagged())
        .partitioned_by(by_tag())
        .commit(
            Commit::new()
                .file(data("x", 2).partition([Value::string("x")]))
                .file(data("y", 2).partition([Value::string("y")]))
                .file(FileSpec::data("n", [row![ID => 9_i64]]).partition([Value::Null])),
        )
        .generate()
        .await
        .unwrap()
}

#[tokio::test]
async fn every_small_file_is_a_candidate() {
    let fixture = partitioned().await;

    let preview = preview(&fixture, PlannerConfig::default()).await;

    assert!(preview.rejections.is_empty());
    assert_eq!(preview.candidates, tally(&fixture, &["n", "x", "y"]));
    assert!(preview.skipped.is_empty());
}

#[tokio::test]
async fn a_table_without_a_snapshot_has_nothing_to_preview() {
    let fixture = TableSpec::new(tagged()).generate().await.unwrap();

    let preview = preview(&fixture, PlannerConfig::default()).await;

    assert_eq!(preview.baseline, None);
    assert_eq!(preview.candidates, FileTally::default());
}

#[tokio::test]
async fn files_at_the_minimum_size_are_candidates_only_with_deletes() {
    // The position delete's `file_path` bounds exclude `a`.
    let fixture = TableSpec::new(tagged())
        .commit(Commit::new().file(data("a", 2)).file(data("b", 2)))
        .commit(Commit::new().file(FileSpec::position_deletes("p", [("b", 0)])))
        .generate()
        .await
        .unwrap();

    let preview = preview(&fixture, deletes_only()).await;

    assert_eq!(preview.candidates, tally(&fixture, &["b"]));
    assert_eq!(
        preview.skipped,
        BTreeMap::from([(SkipReason::AtTargetSize, tally(&fixture, &["a"]))])
    );
}

#[tokio::test]
async fn equality_deletes_apply_only_to_older_data() {
    let fixture = TableSpec::new(tagged())
        .commit(Commit::new().file(data("old", 2)))
        .commit(
            Commit::new()
                .file(data("same", 2))
                .file(FileSpec::equality_deletes("e", [ID], [row![ID => 0_i64]])),
        )
        .commit(Commit::new().file(data("new", 2)))
        .generate()
        .await
        .unwrap();

    let preview = preview(&fixture, deletes_only()).await;

    assert_eq!(preview.candidates, tally(&fixture, &["old"]));
    assert_eq!(
        preview.skipped[&SkipReason::AtTargetSize],
        tally(&fixture, &["new", "same"])
    );
}

#[tokio::test]
async fn position_deletes_apply_to_data_of_the_same_commit() {
    let fixture = TableSpec::new(tagged())
        .commit(
            Commit::new()
                .file(data("d", 2))
                .file(FileSpec::position_deletes("p", [("d", 0)])),
        )
        .commit(Commit::new().file(data("later", 2)))
        .generate()
        .await
        .unwrap();

    let preview = preview(&fixture, deletes_only()).await;

    assert_eq!(preview.candidates, tally(&fixture, &["d"]));
}

#[tokio::test]
async fn partitioned_deletes_apply_only_to_their_partition() {
    let fixture = TableSpec::new(tagged())
        .partitioned_by(by_tag())
        .commit(
            Commit::new()
                .file(data("x", 2).partition([Value::string("x")]))
                .file(data("y", 2).partition([Value::string("y")])),
        )
        .commit(
            Commit::new().file(
                FileSpec::equality_deletes("e", [ID], [row![ID => 0_i64]])
                    .partition([Value::string("y")]),
            ),
        )
        .generate()
        .await
        .unwrap();

    let preview = preview(&fixture, deletes_only()).await;

    assert_eq!(preview.candidates, tally(&fixture, &["y"]));
}

#[tokio::test]
async fn unpartitioned_deletes_apply_to_every_partition() {
    let fixture = TableSpec::new(tagged())
        .partitioned_by(by_tag())
        .commit(
            Commit::new()
                .file(data("x", 2).partition([Value::string("x")]))
                .file(data("y", 2).partition([Value::string("y")])),
        )
        .commit(
            Commit::new()
                .evolve_spec(iceberg::spec::UnboundPartitionSpec::builder().build())
                .file(FileSpec::equality_deletes("e", [ID], [row![ID => 0_i64]])),
        )
        .generate()
        .await
        .unwrap();

    let preview = preview(&fixture, deletes_only()).await;

    assert_eq!(preview.candidates, tally(&fixture, &["x", "y"]));
}

#[tokio::test]
async fn files_over_the_group_limit_are_skipped() {
    let fixture = TableSpec::new(tagged())
        .commit(Commit::new().file(data("big", 50)).file(data("small", 1)))
        .generate()
        .await
        .unwrap();
    let limit = size_of(&fixture, "small");
    assert!(size_of(&fixture, "big") > limit);

    let preview = preview(
        &fixture,
        PlannerConfig {
            target_file_size_bytes: limit,
            min_file_size_bytes: limit,
            max_group_input_bytes: limit,
            ..PlannerConfig::default()
        },
    )
    .await;

    assert_eq!(
        preview.skipped,
        BTreeMap::from([
            (SkipReason::TooLarge, tally(&fixture, &["big"])),
            (SkipReason::AtTargetSize, tally(&fixture, &["small"])),
        ])
    );
}

#[tokio::test]
async fn a_selection_keeps_only_matching_partitions() {
    let fixture = partitioned().await;

    let preview = selected(
        &fixture,
        &[PartitionPredicate::new().with("tag", json!("x"))],
    )
    .await;

    assert_eq!(preview.candidates, tally(&fixture, &["x"]));
    assert_eq!(
        preview.skipped,
        BTreeMap::from([(SkipReason::OutsideSelection, tally(&fixture, &["n", "y"]))])
    );
}

#[tokio::test]
async fn selection_alternatives_are_combined_and_null_matches_null() {
    let fixture = partitioned().await;

    let preview = selected(
        &fixture,
        &[
            PartitionPredicate::new().with("tag", json!("y")),
            PartitionPredicate::new().with("tag", json!(null)),
        ],
    )
    .await;

    assert_eq!(preview.candidates, tally(&fixture, &["n", "y"]));
}

#[tokio::test]
async fn a_spec_scoped_predicate_ignores_other_specs() {
    let fixture = TableSpec::new(tagged())
        .partitioned_by(by_tag())
        .commit(Commit::new().file(data("x", 2).partition([Value::string("x")])))
        .commit(
            Commit::new()
                .evolve_spec(iceberg::spec::UnboundPartitionSpec::builder().build())
                .file(data("plain", 2)),
        )
        .generate()
        .await
        .unwrap();

    let only_plain = selected(&fixture, &[PartitionPredicate::new().in_spec(1)]).await;
    let only_x = selected(
        &fixture,
        &[PartitionPredicate::new().in_spec(0).with("tag", json!("x"))],
    )
    .await;

    assert_eq!(only_plain.candidates, tally(&fixture, &["plain"]));
    assert_eq!(only_x.candidates, tally(&fixture, &["x"]));
}

#[tokio::test]
async fn invalid_selections_are_refused() {
    let fixture = partitioned().await;
    let cases = [
        (
            PartitionPredicate::new().with("region", json!("x")),
            "no partition spec",
        ),
        (
            PartitionPredicate::new().with("tag", json!(1)),
            "not a string value",
        ),
        (
            PartitionPredicate::new().in_spec(7),
            "spec 7 does not exist",
        ),
    ];

    for (predicate, expected) in cases {
        let error = preview_with(
            &fixture,
            fixture.metadata(),
            PlannerConfig::default(),
            &[predicate],
        )
        .await
        .unwrap_err();

        assert!(
            matches!(&error, PlanError::InvalidSelection(message) if message.contains(expected)),
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn metadata_rejections_are_all_reported() {
    let fixture = partitioned().await;
    let metadata = with_properties(
        fixture.metadata(),
        &[("encryption.key-id", "k"), ("write.format.default", "orc")],
    );
    let order = SortOrder::builder()
        .with_sort_field(SortField {
            source_id: ID,
            transform: Transform::Identity,
            direction: SortDirection::Ascending,
            null_order: NullOrder::First,
        })
        .build_unbound()
        .unwrap();
    let metadata = TableMetadataBuilder::new_from_metadata(metadata, None)
        .add_sort_order(order)
        .unwrap()
        .set_default_sort_order(-1)
        .unwrap()
        .build()
        .unwrap()
        .metadata;

    let preview = preview_with(&fixture, &metadata, PlannerConfig::default(), &[])
        .await
        .unwrap();

    assert_eq!(
        preview.rejections,
        [
            Rejection::SortedTable { sort_order_id: 1 },
            Rejection::Encrypted,
            Rejection::WriteFormat {
                format: "orc".into()
            },
        ]
    );
    assert_eq!(preview.candidates, FileTally::default());
    assert!(
        preview.rejections[0]
            .to_string()
            .contains("sorting is not supported"),
        "{}",
        preview.rejections[0]
    );
}

#[tokio::test]
async fn parquet_write_format_is_accepted_in_any_case() {
    let fixture = partitioned().await;
    let metadata = with_properties(fixture.metadata(), &[("write.format.default", "PARQUET")]);

    let preview = preview_with(&fixture, &metadata, PlannerConfig::default(), &[])
        .await
        .unwrap();

    assert!(preview.rejections.is_empty());
}

#[tokio::test]
async fn format_versions_other_than_two_are_refused() {
    let fixture = partitioned().await;
    let metadata: TableMetadata = TableMetadataBuilder::new(
        tagged(),
        PartitionSpec::unpartition_spec(),
        SortOrder::unsorted_order(),
        fixture.metadata().location().to_owned(),
        FormatVersion::V1,
        HashMap::new(),
    )
    .unwrap()
    .build()
    .unwrap()
    .metadata;

    let preview = preview_with(&fixture, &metadata, PlannerConfig::default(), &[])
        .await
        .unwrap();

    assert_eq!(
        preview.rejections,
        [Rejection::FormatVersion { version: 1 }]
    );
    assert!(preview.rejections[0].to_string().contains("only v2"));
}

#[tokio::test]
async fn floating_point_equality_keys_are_refused() {
    const SCORE: i32 = 2;
    let fixture = TableSpec::new(schema([
        NestedField::required(ID, "id", Type::Primitive(PrimitiveType::Long)),
        NestedField::optional(SCORE, "score", Type::Primitive(PrimitiveType::Double)),
    ]))
    .commit(Commit::new().file(FileSpec::equality_deletes(
        "e",
        [SCORE],
        [row![SCORE => Value::double(1.0)]],
    )))
    .generate()
    .await
    .unwrap();

    let preview = preview(&fixture, PlannerConfig::default()).await;

    assert_eq!(
        preview.rejections,
        [Rejection::EqualityKey {
            path: fixture.path_of("e").unwrap().to_owned(),
            field_id: SCORE,
            problem: KeyProblem::FloatingPoint,
        }]
    );
    assert!(preview.rejections[0].to_string().contains("floating-point"));
}

#[tokio::test]
async fn equality_keys_on_dropped_fields_are_refused() {
    let fixture = TableSpec::new(tagged())
        .commit(Commit::new().file(FileSpec::equality_deletes("e", [TAG], [row![TAG => "x"]])))
        .commit(Commit::new().evolve_schema(schema([NestedField::required(
            ID,
            "id",
            Type::Primitive(PrimitiveType::Long),
        )])))
        .generate()
        .await
        .unwrap();

    let preview = preview(&fixture, PlannerConfig::default()).await;

    assert!(
        matches!(
            preview.rejections.as_slice(),
            [Rejection::EqualityKey {
                field_id: TAG,
                problem: KeyProblem::NotInSchema,
                ..
            }]
        ),
        "{:?}",
        preview.rejections
    );
}

#[tokio::test]
async fn the_inventory_is_bounded() {
    let fixture = partitioned().await;

    let error = preview_with(
        &fixture,
        fixture.metadata(),
        PlannerConfig {
            max_manifest_entries: 2,
            ..PlannerConfig::default()
        },
        &[],
    )
    .await
    .unwrap_err();

    assert!(
        matches!(error, PlanError::InventoryTooLarge { limit: 2 }),
        "{error:?}"
    );
}

#[test]
fn inconsistent_configurations_are_refused() {
    let base = PlannerConfig::default();
    let cases = [
        PlannerConfig {
            target_file_size_bytes: 0,
            min_file_size_bytes: 0,
            ..base.clone()
        },
        PlannerConfig {
            min_file_size_bytes: base.target_file_size_bytes + 1,
            ..base.clone()
        },
        PlannerConfig {
            max_group_input_bytes: base.target_file_size_bytes - 1,
            ..base.clone()
        },
    ];

    for config in cases {
        assert!(
            matches!(
                bergsmith_maintenance::Planner::new(config.clone()),
                Err(PlanError::InvalidConfig(_))
            ),
            "{config:?}"
        );
    }
}
