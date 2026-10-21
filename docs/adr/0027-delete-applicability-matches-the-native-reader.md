# 0027. Delete applicability matches the native reader's metric proof

- Status: Accepted
- Date: 2026-10-19
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §3 Delete-aware compaction; §4 BS-011
- Extends ADR 0020; supersedes the pinned revision named in ADR 0021

## Context

The executor requires the deletes iceberg-rust's scan planner attaches to each
source to equal Bergsmith's own applicable set (ADR 0018). At the pinned
revision, that planner also excludes deletes that column metrics prove cannot
apply (`can_contain_eq_deletes_for_file` and `can_contain_pos_deletes_for_file`
in `delete_file_index.rs`). Bergsmith's index did not, so every table written
with column metrics, as Spark and Flink write them, failed preparation with
`PlannedFileIdentityMismatch`. Fixtures recorded only `file_path` bounds, so no
test exercised the proof.

Two scope details also differ. Iceberg, like Java's
`PartitionSpec::isUnpartitioned`, treats an equality delete in a spec whose
fields are all `void` as global; the pinned planner treats only a spec without
fields as global. And the reader resolves a column absent from a data file by
the spec's column-projection rules (identity-partition value, then
`initial-default`, then null), while the oracle always read null.

ADR 0021 names revision `eb162a1`; commit `1b4bfe4` moved the pin to
`e7ed9e8`, which adds hash-based equality deletes (see `docs/dependencies.md`).

## Decision

- Reproduce the pinned planner's proof exactly in the planner and executor's
  shared delete index. An equality delete is excluded only when some key field
  cannot hold a null in both files and the data and delete bounds are disjoint.
  A referenced data file scopes a position delete exactly; otherwise valid
  `file_path` bounds exclude other paths, and reversed bounds prove nothing.
  Missing or differently typed metrics never exclude a delete. The executor
  keeps requiring an exact match with the reader's attachments.
- The inventory keeps column metrics only for fields that a live equality
  delete is keyed on, reading delete manifests first.
- The independent oracle never uses metrics. Exclusions are sound: a match is
  impossible, so the oracle's row result is unchanged. Fixtures can record
  engine-style column metrics (`TableSpec::with_column_metrics`), and native
  tests compare such tables with the oracle.
- Keep Iceberg's global semantics for all-`void` specs. When such an equality
  delete applies to data in another spec, the planner and executor reject the
  table with `VoidSpecEqualityDelete` before any row read, because the native
  reader would not apply it and rows would be resurrected.
- The oracle follows the column-projection rules for data files, and fixtures
  can write data files without some columns (`FileSpec::without_fields`).
- The pin is `thealtoclef/rw-iceberg-rust@28b0f37c446e154d95860d3851eb455d0099cac9`,
  advanced from `e7ed9e8` for the Arrow/Parquet 59.3.1 upgrade that removes
  Thrift 0.17 (CVE-2026-43868), and recorded with its evidence in
  `docs/dependencies.md`. ADR 0021's other decisions remain in force.

## Consequences

Tables written with column metrics compact, and pruned deletes no longer
inflate planning cost or delete reservations. Advancing the iceberg-rust pin
requires rechecking the proof functions against this index. Tables with
cross-spec all-`void` equality deletes stay unsupported until the reader
applies them globally.
