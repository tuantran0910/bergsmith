# 0020. Delete scope and retention during native rewrites

- Status: Accepted
- Date: 2026-10-17
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §3 Delete-aware compaction; §4 BS-011

## Context

The extended native correctness suite exposed an error in Bergsmith's planner
and independent oracle: both treated unpartitioned position-delete files as
global. iceberg-rust correctly scopes those files to the original partition
spec and values, so the executor refused the inconsistent planned inventory.

The capture-catalog rewrite action also enables upstream delete cleanup by
default. A zero-output rewrite with an equality delete older than all live data
silently retired that delete from the new snapshot. This exceeds the MVP's
authority to replace selected data while retaining deletes.

Finally, the pinned upstream action validates added files against the default
partition spec and writes their manifest using that spec. This prevents
publication of correctly executed replacements under an older spec.

## Decision

- Correct the position-delete scope stated in ADR 0016: both partition-spec ID
  and partition values must match. A matching path alone does not widen scope.
  Only equality deletes in an unpartitioned spec are global, as required by
  [Iceberg scan planning](https://iceberg.apache.org/spec/#scan-planning).
  Position deletes apply at equal or newer data sequence numbers; equality
  deletes apply only at strictly newer sequence numbers.
- Keep the planner and independent oracle implementations separate and test
  each against explicit expected rows, including spec evolution.
- Explicitly disable the rewrite action's delete filter manager. Preserve all
  delete descriptors and their original sequence numbers, even when obsolete
  or when the rewritten group produces no data files. Delete retirement remains
  a separate future operation.
- Added data files retain the planning snapshot's data sequence number. Their
  file sequence number belongs to the newly published snapshot. Check both in
  the written Avro manifests, alongside unchanged shared deletes and untouched
  data, and compare exact live-row multisets after applying captured updates.
- Prepare the upstream old-spec correction as an isolated patch: validate each
  added file against its own known spec and write one added manifest per spec.
  Keep original baseline requirements and the default spec unchanged. Hosting,
  dependency pinning, and validation are recorded in [dependencies](../dependencies.md).

## Consequences

The native suite covers null patterns in composite keys, duplicate survivors,
position/equality sequence boundaries, global equality scope, float/double NaN
payload preservation, unsupported equality-key rejection, zero-output groups,
and retained-delete publication semantics. No equality-filter performance
patch is needed: the BS-010 release samples fit the measured duration budget.

The publication fixtures execute actual Parquet and inspect real Avro manifests
through the capture catalog. They apply captured updates locally; durable
receipt acceptance, send authorization, and unknown-outcome recovery remain
BS-012. Process isolation remains BS-015.
