//! The oracle reproduces the expected live rows of all 11 spike fixtures
//! (BS-008 done condition).

// clippy's `allow-unwrap-in-tests` covers only `#[test]` functions; the
// helpers below fail the test on setup errors.
#![expect(clippy::unwrap_used, reason = "test helpers fail the test")]

use std::collections::BTreeSet;

use bergsmith_fixtures::spike::{self, SpikeFixture};

async fn assert_reproduced(fixture: SpikeFixture, expected_rows: u64) {
    let table = fixture.generate().await.unwrap();

    let live = fixture.oracle_rows(&table).await.unwrap();

    assert_eq!(fixture.expected.len(), expected_rows, "{}", fixture.name);
    assert_eq!(
        live,
        fixture.expected,
        "{}: oracle has extra {:?}, misses {:?}",
        fixture.name,
        live.excess_over(&fixture.expected).first(),
        fixture.expected.excess_over(&live).first()
    );
}

#[tokio::test]
async fn small_files_with_duplicates_and_nulls() {
    assert_reproduced(spike::small_files_with_duplicates_and_nulls().unwrap(), 40).await;
}

#[tokio::test]
async fn position_deletes_across_row_groups() {
    assert_reproduced(spike::position_deletes_across_row_groups().unwrap(), 18).await;
}

#[tokio::test]
async fn equality_deletes_with_old_and_same_sequence_data() {
    assert_reproduced(
        spike::equality_deletes_with_old_and_same_sequence_data().unwrap(),
        6,
    )
    .await;
}

#[tokio::test]
async fn nullable_single_equality_key() {
    assert_reproduced(spike::nullable_single_equality_key().unwrap(), 1).await;
}

#[tokio::test]
async fn composite_nullable_equality_keys() {
    assert_reproduced(spike::composite_nullable_equality_keys().unwrap(), 3).await;
}

#[tokio::test]
async fn shared_deletes_with_an_untouched_file() {
    assert_reproduced(spike::shared_deletes_with_an_untouched_file().unwrap(), 12).await;
}

#[tokio::test]
async fn fully_deleted_group() {
    assert_reproduced(spike::fully_deleted_group().unwrap(), 0).await;
}

#[tokio::test]
async fn original_spec_with_a_different_default() {
    assert_reproduced(spike::original_spec_with_a_different_default().unwrap(), 15).await;
}

#[tokio::test]
async fn renamed_field_and_added_column() {
    assert_reproduced(spike::renamed_field_and_added_column().unwrap(), 3).await;
}

#[tokio::test]
async fn large_equality_delete() {
    assert_reproduced(spike::large_equality_delete().unwrap(), 50_000).await;
}

#[tokio::test]
async fn wide_rows() {
    assert_reproduced(spike::wide_rows().unwrap(), 32_768).await;
}

#[test]
fn the_suite_has_all_11_fixtures() {
    let names: BTreeSet<&str> = spike::all().unwrap().iter().map(|f| f.name).collect();

    assert_eq!(names.len(), 11);
}
