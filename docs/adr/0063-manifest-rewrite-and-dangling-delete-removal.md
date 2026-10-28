# 0063. Manifest rewrite and dangling-delete removal

- Status: Proposed
- Date: 2026-10-28
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-031.T3

## Context

Two kinds of metadata debt build up on a maintained table and nothing
removes them.

- **Small manifests.** Every append writes at least one data manifest. A table
  written in small commits ends up with thousands of tiny manifests, and every
  scan plan reads all of them. The health model already reports this as
  `ManifestFragmentation` (ADR 0059), with no action to resolve it.
- **Dangling delete files.** Compaction applies the deletes that match its
  inputs but keeps every delete file (ADR 0020), because rewrite authority
  covers only the selected data files. Once no live data file is in a delete's
  scope, the delete can never match a row, but readers still open it for
  every planned scan. The health model reports the bytes as `DeletePressure`.

Both repairs change only metadata. Neither reads or writes table rows, and
both can be expressed as one standard Iceberg commit that adds a `replace`
snapshot on `main`.

## Decision

**Two new coordinator-only job operations.** `RewriteManifests`
(`rewrite_manifests` in the API) and `RemoveDanglingDeletes`
(`remove_dangling_deletes`) are maintenance kinds, like `ExpireSnapshots`
(ADR 0061). The coordinator plans them from the catalog's metadata and
manifests, persists an immutable hashed plan, and moves the job
`Planning -> ReadyToCommit` directly. Workers never see them.

**One immutable plan table for both.** Migration 0038 adds
`metadata_plans(job_id, content, content_hash)`, guarded by the same
`bergsmith_reject_mutation` trigger as every other plan table. The document
records its operation, the planning baseline (`PlanningBaseline`), and the
exact inputs. Its hash covers the canonical typed form.

**Selection: manifest rewrite.** The candidates are the data manifests of the
baseline `main` snapshot that are smaller than the target size: the table's
`commit.manifest.target-size-bytes`, or Iceberg's 8 MiB default. Delete
manifests are never rewritten (the pinned iceberg-rust action cannot), and a
manifest whose file counts are unknown is skipped. Candidates are grouped by
partition spec; a spec with fewer than two candidates has nothing to merge.
The plan lists every selected manifest's path, length, spec, and live file
count. No selection ends the job `NoOp` (`NoManifestsToRewrite`).

**Selection: dangling deletes.** A delete file is dangling when it applies to
no live data file in the baseline `main` snapshot. Applicability is the
planner's own Iceberg v2 delete index (ADR 0027): partition and spec scope,
sequence numbers (position deletes at `>=`, equality deletes at `>`),
`referenced_data_file`, `file_path` bounds, and equality-key null counts and
bounds. Using column bounds as proof is deliberate: the reader excludes the
same deletes from every scan, so removing them changes no read. Bounds never
prove anything for a floating-point key: NaN has no place in an ordering, so
those deletes are judged by scope and sequence numbers alone. Missing metrics
keep a delete. The candidates are sorted by path and truncated to
`deletes.max_removals`; the rest wait for a later job. No candidate ends the
job `NoOp` (`NoDanglingDeletes`).

**Publication reuses the rewrite evidence path.** Both operations publish a
new `main` snapshot with an explicit snapshot ID, the publication ID as the
iceberg-rust commit UUID, and the `bergsmith.job-id`, `bergsmith.publication-id`
and `bergsmith.proposal-sha256` summary properties. The intent has the same
shape as a compaction intent: the plan, no receipts, and the expected
snapshot with its manifest list. The send, the outcome mapping, ambiguous
recovery by snapshot evidence, operator attestation, orphan protection of the
intent's manifests (ADR 0062), and expiration pinning of the baseline
(ADR 0061) therefore all work unchanged.

- Manifest rewrite uses iceberg-rust's `RewriteManifestsAction` through the
  capture catalog (ADR 0015). It rewrites exactly the planned manifests
  (`rewrite_if` by path), packs each spec's entries into target-sized outputs
  under one constant cluster key, and fails if the action's own count check
  finds a different number of live files. Load and write concurrency are
  fixed and small.
- Dangling-delete removal uses the same `rewrite_files` action as compaction,
  with the planned delete files removed and nothing added. Each planned path
  is resolved to its baseline manifest entry first; a missing or duplicated
  entry fails the preparation.

**The original baseline is required.** Preparation and publication recheck
`PlanningBaseline` equality, and the commit carries the standard
`assert-ref-snapshot-id` requirement on `main`. Any change (a new append, a
compaction, an expiration of `main`'s head) ends the job `ReplanRequired`.
The first release never rebases these operations onto a newer head: each one
is cheap to plan again, and a stale dangling proof is unsafe.

**A table with live compaction refuses them.** Submitting either operation
while the table has a nonterminal `Compact` job answers 409
`compaction_in_progress`. The publication barrier and the baseline
requirement already make the race safe; the refusal keeps a metadata commit
from invalidating a compaction whose workers have already rewritten data.

**Both are off by default.** `manifests.rewrite_enabled` and
`deletes.dangling_removal_enabled` must be set. While one is off, submission
and retry answer 400 (`manifest_rewrite_unavailable`,
`dangling_delete_removal_unavailable`), planning fails the job, and a
prepared but unsent intent fails; an authorized send still recovers from
evidence. The switches let an operator adopt each repair separately.

**Every step is bounded.** Manifest rewrite takes at most
`manifests.max_input_manifests` manifests recording at most
`manifests.max_input_entries` live files, spec by spec in path order; a spec
left with a single selected manifest is dropped, and the rest wait for a later
job. The plan records how many eligible manifests it deferred. The
iceberg-rust action is one opaque step, so the whole preparation runs under
`manifests.operation_timeout_seconds`. Dangling-delete selection reuses the
planner's manifest-entry bound, and its inventory read with classification,
and its preparation, each run under `deletes.operation_timeout_seconds`.
Classification tests each delete only until it first applies, stops once
every delete is accounted for, and yields regularly so the deadline can stop
it; a selection that completes after its deadline is rejected. A missed
deadline is retried like any transient storage failure.

## Consequences

- Manifest fragmentation and dangling-delete pressure now have actions. The
  health recommendations name them; they stay advisory while the operations
  are disabled.
- Both operations need no worker capacity and finish in one planning pass and
  one publication pass.
- A hot table whose `main` moves faster than plan-to-publish can starve both
  operations with `ReplanRequired`. Rebasing onto a newer head would need a
  fresh dangling proof and a fresh manifest selection, which is a new plan
  anyway.
- Dangling-delete removal only examines `main`. A delete still referenced by
  a branch or tag snapshot stays there; removing it from `main` changes only
  `main`'s new snapshot.
- The replaced manifests and delete files stay in storage while any retained
  snapshot references them. Snapshot expiration (ADR 0061) and orphan cleanup
  (ADR 0062) reclaim them later.
- Deletes whose scope cannot be proven empty from recorded metrics are kept,
  so engines that write sparse metrics leave more deletes behind.

## Implementation

- `bergsmith-maintenance` adds manifest selection and dangling-delete
  selection with `MetadataPlanDocument`, and their unit tests.
- `bergsmith-catalog` adds `ManifestRewriteProposal`, which builds the
  `RewriteManifestsAction` commit and its expected snapshot.
- Migration 0038 adds the operation values and `metadata_plans`; persistence
  stores the plan in the fenced planning transaction and pins its baseline
  against expiration.
- The planning pass builds the plan; the publication pass prepares the
  intent and sends it once under send authorization.
- `POST /api/v1/jobs` accepts both operations; the table page offers them
  behind confirmation, and the job pages label them.
