# 0003. Bergsmith-owned executor on iceberg-rust, without DataFusion

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §3 Reference implementation; §3 Native executor

## Context

`nimtable/iceberg-compaction` offers a DataFusion-based rewrite executor. A spike found a null-key equality-delete defect in it, and its managed workflow conflicts with Bergsmith's commit and recovery contracts.

## Decision

- Bergsmith owns its executor in `crates/maintenance`, built on iceberg-rust's Arrow reader, delete filter, and Parquet writers at the pinned revision.
- `nimtable/iceberg-compaction` is a design and test reference only, never a dependency.
- DataFusion is excluded from the MVP: binpack compaction needs no sort, join, or aggregation.

## Consequences

- One delete-application path to certify; smaller build.
- Bergsmith owns delete correctness and must certify iceberg-rust's delete filter (BS-011).
- Equality deletes are a predicate tree; BS-010 benchmarks large delete sets, and a hash-based filter may become an iceberg-rust patch.
- DataFusion is reconsidered when sorting arrives (M9).
