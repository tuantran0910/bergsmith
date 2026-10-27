# 0062. Orphan-file cleanup with reachability and ledger protection

- Status: Proposed
- Date: 2026-10-27
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-031.T2

## Context

Snapshot expiration (ADR 0061) removes snapshot entries from table metadata and
deletes nothing. Data, manifest, manifest-list, statistics, and metadata files
that no retained snapshot references stay in object storage forever. They are
invisible to readers, so nothing removes them; the storage bill keeps growing.

Deleting storage objects is irreversible and unlike a catalog commit: there is
no server-side conditional write, no idempotency guarantee to appeal to, and no
publication to be ambiguous about. A cleanup that guesses wrong destroys data a
reader, a running job, or a late commit still needs. Bergsmith also writes its
own artifacts under `<table-location>/bergsmith/`, which are never table data
and must never be treated as orphans.

## Decision

**Orphan cleanup is a coordinator job with no catalog commit and no worker
tasks.** It is a new job operation, `RemoveOrphanFiles`. The coordinator lists
the table's storage, proves which objects nothing needs, records an immutable
plan, and deletes exactly that plan. Workers never see the job.

**A deletion has its own lifecycle state.** `ReadyToCommit` means the plan is
durable; `Deleting` means deletion is authorized and in progress;
`Deleting -> Succeeded | Cancelling | Failed` complete it. The state exists so
the publication machinery's ambiguity rules (`Committing`,
`CommitOutcomeUnknown`, send authorization, attestation) are never reused for a
local idempotent side effect. A deletion cannot publish, so no state after
`Deleting` asserts or requires publication evidence.

**Reachability is computed from the table, never assumed.** The protected set
is the union of:

- the current metadata file location;
- every metadata file listed in `metadata-log`, kept so rollback and audit
  still work;
- every snapshot's manifest list, every manifest it names, and every data or
  delete file in those manifests, across every snapshot the current metadata
  retains;
- every statistics file and partition statistics file the metadata names.

Everything else under the table location is a candidate. Objects outside the
table location are never deleted, even when metadata references them.

**Bergsmith's own tree is never a candidate.** No object within
`<table-location>/bergsmith/` is listed for deletion, so attempt outputs,
manifests a job is writing, and artifacts an unresolved publication will add
survive. Files an unresolved job may publish are therefore protected twice:
their outputs are under that prefix, and the deletion pass reloads table
metadata so a commit that landed protects its files as reachable.

**Only old, unreferenced objects are candidates.** The age floor is the
stricter of `orphans.minimum_age_seconds` (default three days, matching
Iceberg's client default) and the connection's
`max_execution_seconds + max_unresolved_publication_seconds` protection window.
An object whose last-modified time is unknown is retained. The candidates are
sorted by last-modified time and location, then truncated to
`orphans.max_candidates` (oldest first); the rest wait for a later job.

**The plan is immutable and hashed.** `OrphanPlanDocument` records the
planning baseline (table UUID, metadata location, current snapshot), the
effective cutoff, the listed and protected counts, and the exact candidate
paths with sizes and modification times. Its hash covers the canonical typed
form, as with compaction and expiration plans. A rerun of the planner cannot
change what a job deletes.

**Deletion re-verifies before it removes.** For each bounded batch the leader
reloads table metadata, recomputes reachability, and deletes a candidate only
while it is still unreferenced, still older than the effective window, still
outside the Bergsmith prefix, and still inside the table location. A candidate
that fails a check is recorded as skipped, never deleted. Deletes are
idempotent, so a leader that dies between the delete and the receipt only
repeats itself.

**Ledger receipts make the job resumable and auditable.**
`orphan_deletions` records one row per planned candidate with its disposition
(`Deleted` or `Skipped`), size, and time. Completion requires a receipt for
every planned candidate; the job then moves to `Succeeded`. A pass is bounded
by `orphans.max_deletions_per_pass`, so a large plan continues over several
passes and a restart resumes where the receipts stop.

**Deletion is off by default.** `orphans.enabled` must be set: the submission
API answers 400 while it is false, and planning and the first deletion fail
closed if it was disabled between them. Listing and scanning are bounded by
`orphans.max_listed_objects` and `orphans.max_scan_entries`; exceeding either
fails the job permanently with a typed reason instead of deleting from a
partial listing.

**Cancellation stays provable.** A deletion may be cancelled before it starts
or between batches; already-removed objects were unreferenced, so the job
finishes as `Cancelled` with its partial receipts. Cancellation never claims
publication.

## Consequences

- Unreferenced storage is reclaimed under an explicit operator switch, with an
  immutable plan, per-object evidence, and a resumable state machine.
- Expiration plus cleanup now recover storage; the storage breakdown (T4) can
  attribute reclaimed bytes.
- Deleting an object cannot be undone. The age floor, the prefix rule, the
  metadata reload, and the receipts bound the risk but do not make the
  operation safe to run unattended on a table whose writers misbehave: the
  first release requires an operator to enable it.
- `Deleting` is a new job state in the shared lifecycle, the protocol enum, and
  the API schema. Every state consumer (metrics, UI, filtering) must know it.
- Historical metadata files in `metadata-log` are protected, but their
  referenced data files are not: this pin cannot parse an older metadata file.
  Files that only an older metadata version references can be removed, as with
  Iceberg's own orphan-file action. Rollback after cleanup reads the metadata
  file but may find expired files missing.
- A very large table can exceed the scan or listing bounds; the first release
  fails closed rather than paging the reachability scan. Later work can raise
  the bounds or page them.

## Implementation

- `bergsmith-maintenance` holds reachability capture (bounded manifest scan),
  the pure selection function, and `OrphanPlanDocument` with its tests.
- Migration 0037 adds the `RemoveOrphanFiles` operation value, the `Deleting`
  job state, immutable `orphan_plans`, and `orphan_deletions` receipts.
- `bergsmith-persistence` stores plans and receipts inside the fenced planning
  and deletion transactions.
- The planning pass builds the plan and moves the job to `ReadyToCommit`; the
  publication pass executes bounded batches, records receipts, and completes
  the job. Both are leader-only.
- `POST /api/v1/jobs` accepts `operation: "remove_orphan_files"` while
  `orphans.enabled` is set; the table page offers the action, and the job page
  labels it.
