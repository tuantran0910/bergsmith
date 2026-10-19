# 0018. Verify planned identities before native row scans

- Status: Accepted; inventory bound superseded by 0028
- Date: 2026-10-17
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §3 Native executor; BS-010.T1

## Context

Workers must execute immutable planned files without catalog access. A whole-table
scan could accidentally rewrite unselected files, and a missing or changed delete
descriptor could resurrect rows. Snapshot schemas can also lag metadata-only
schema changes captured by the planner.

## Decision

- `Executor::prepare` accepts a task artifact, the trusted digest from its grant,
  pinned table metadata, and storage-only `FileIO`. The caller must load the
  original metadata file; it must never refresh the table through a catalog.
- Preparation verifies the artifact digest, table UUID and location, format,
  snapshot ID and sequence, and current schema ID. It checks source and delete
  identities against the pinned manifests: path, size, row count, data sequence,
  partition spec and values, delete content, equality keys, and referenced file.
- Sources must belong to the output partition group. Duplicate identities,
  missing sources, omitted applicable deletes, and unrelated deletes are errors.
- Native metadata scan planning supplies partition constants and name mappings.
  Keep only the artifact's exact source paths, verify each source's attached
  delete inventory, and project all fields of the pinned current schema by ID.
  Selection predicates never become row filters.
- `PreparedScan` has a private constructor and owns the verified scans. It exposes
  a lazy Arrow reader using iceberg-rust's existing delete filter. It cannot
  publish, write outputs, or delete files.
- Initial preparation has a 60-second deadline and the default planner inventory
  limit. Reader defaults are 8,192 rows per batch, one data file at a time, and
  four range fetches per file. These are initial implementation defaults, pending
  BS-010.T3 resource budgets and BS-010.T5 measurements. The caller must bound lazy
  stream consumption with I/O timeouts.

## Consequences

Identity checks complete before any data or delete rows are read. Preparation
reads manifests twice, once for explicit identity checks and once through the
native scan planner; this trades extra metadata I/O for reuse of native schema
and partition handling. It does not replan the task or change its artifact.

The API requires trusted pinned metadata alongside the hashed artifact. Local
execution and supervisor wiring must supply that context from the immutable
plan's metadata location; the current worker protocol has not yet wired it.

Output writers and receipts remain BS-010.T2, resource enforcement remains T3,
and complete delete certification remains BS-011. Preparing all fixture scans
does not certify their delete results or enable production maintenance.
