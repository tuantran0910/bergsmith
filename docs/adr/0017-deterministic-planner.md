# 0017. A deterministic, metadata-only planner

- Status: Accepted
- Date: 2026-10-16
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §3 Planning and execution; §3 Delete-aware compaction; §4 BS-009

## Context

A job's plan is immutable once persisted and is the only input workers
execute. Its hash identifies it in grants, receipts, and recovery, so the
same table state must always give the same plan. The coordinator must never
read rows (architecture invariant 2), and conservative conflict rejection
means a job succeeds only if it fits between two commits of the table's
writers (ADR 0010). The MVP does not sort, and some delete keys cannot be
compared safely.

## Decision

- The planner lives in `bergsmith-maintenance` and reads only table
  metadata, the `main` manifest list, and manifests. The coordinator
  persists its output; the maintenance crate has no database access.
- The planning baseline records the table UUID, metadata location, format
  version, `main` snapshot and sequence number, schema, spec, and sort-order
  IDs, the last assigned field and partition IDs, the data location, and the
  plan-relevant properties shared with the catalog drift check.
- Preview rejects, with a typed reason each: a format version other than 2,
  a default sort order, encryption, a non-Parquet write format or live file,
  and equality deletes keyed on floating-point, nested, or missing fields.
- A data file is a candidate if it is under 75% of the target size or has an
  applicable delete. Files over the group input limit are never selected.
  Partition predicates select whole files and never become row filters.
- Applicable deletes follow the v2 sequence and partition-scope rules on
  metadata. `referenced_data_file` and `file_path` bounds narrow position
  deletes. Over-inclusion is allowed because the executor matches deletes by
  path and key.
- Candidates are grouped by spec ID and partition (Iceberg single-value
  bytes), and packed first-fit-decreasing by size, then path, into bins of at
  most the group input limit.
- A bin is beneficial if it has at least 5 files and removes at least 2, or
  if it has applicable deletes. Bins are admitted, most files removed first,
  within the job input, file, and duration limits. The duration estimate is
  Graham's list-scheduling bound over the concurrent group limit, so no
  dispatch order exceeds it. Throughput is configured (64 MiB/s per task
  until BS-010 measures it).
- A due job waits up to 6 minutes for a new `main` snapshot, then plans
  against the latest head whether or not one appeared.
- The plan document is canonical JSON from typed, ordered structures. Its
  hash is domain-separated SHA-256 and is re-checked when read back from
  `jsonb`. Task IDs are UUIDv8 values derived from the plan ID and ordinal.
  Each task records its artifact digest. A plan and the job's move to
  `Queued` commit in one transaction.

## Consequences

- Plans are reproducible: property tests shuffle manifest order and check
  the plan hash, and limit tests check every bound.
- Sorted tables cannot be compacted until sorting arrives (M9).
- Oversized files are never split in the MVP.
- The duration budget is only as good as the throughput figure; BS-010
  replaces the default with a measured value.
- Planning cost grows with the number of live files, bounded by a
  configurable manifest-entry limit.
