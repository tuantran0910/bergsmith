# 0016. Native fixtures and an independent live-row oracle

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §3 Native executor; §3 Rewrite-only spike findings; §4 BS-008

## Context

The executor (BS-010) and delete handling (BS-011) must preserve the exact
multiset of live rows. A row-count check misses duplicated or wrongly deleted
rows, and the reference spike found a delete defect that only an exact
comparison caught. Tests therefore need real Iceberg v2 tables with chosen
sequence numbers, partition specs, schema evolution, and delete files, and a
way to compute the expected live rows that does not share code with the
executor.

The spike's fixtures and Python oracle were deleted after the run, so only
their descriptions and expected row counts remain.

## Decision

- Fixtures and the oracle live in a dev-only crate, `crates/fixtures`
  (`bergsmith-fixtures`). Only test and bench targets depend on it.
- A fixture is a declaration: ordered commits, each optionally evolving the
  schema or default partition spec and adding data, position-delete, or
  equality-delete files with stated specs, partition values, and contents.
  A commit's snapshot ID, sequence number, and timestamp follow from its
  position. Files that should share a sequence number go in one commit.
  The generator writes it to a local directory as Parquet with field IDs, v2
  manifests, manifest lists, and `metadata.json`, using iceberg-rust's spec
  writers. It needs no catalog or object store; `FileIO::new_with_fs` reads
  it.
- Generation is deterministic. The same declaration produces the same paths,
  Parquet bytes, and parsed metadata.
- The oracle reads Parquet with the `parquet` crate's Arrow reader and
  applies the v2 delete rules itself. It never calls iceberg-rust's Arrow
  reader or delete filter, which are what BS-010 and BS-011 certify. It does
  use iceberg-rust to parse table metadata and manifests.
- The oracle's rules:
  - A position delete applies when the path matches, the data sequence
    number is at most the delete's, and the partition matches or the
    delete's spec is unpartitioned.
  - An equality delete applies when the data sequence number is strictly less
    than the delete's, the partition matches or the delete's spec is
    unpartitioned, and every key is equal, with null equal to null.
  - Float, double, and nested equality keys are refused.
  - Columns match by field ID. A column a file lacks reads as null. Int to
    long and float to double promotions are applied.
- Rows are compared as multisets keyed by field ID. Floats compare by bit
  pattern, so NaN values are kept and counted.
- Each spike fixture declares its expected rows separately from the oracle,
  and the acceptance suite asserts that the oracle reproduces them.

## Consequences

- The executor can be tested without Compose services, so BS-009 to BS-011
  run in the fast test tier.
- An iceberg-rust metadata parsing bug would affect the oracle and the code
  under test alike. BS-010 compares source objects and output metadata
  separately.
- Manifest bytes are not reproducible, because Avro sync markers are random.
  Determinism is checked on Parquet bytes and parsed metadata.
- The reconstructed fixtures match the spike's descriptions and row counts,
  not its exact data.
