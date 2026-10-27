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
- every object identity as `(backend, authority, key)`, so `s3` and `s3a`
  spellings of one object compare equal;
- every statistics file and partition statistics file the metadata names;
- every manifest list, manifest, and data file named by a rewrite publication
  intent whose job is `ReadyToCommit`, `Committing`, or
  `CommitOutcomeUnknown`. Those manifests live under the metadata directory and
  no snapshot references them, so reachability alone cannot see them.

Everything else under the table location is a candidate. Objects outside the
table location are never deleted, even when metadata references them.

**Bergsmith's own tree is never a candidate.** Attempt outputs, task artifacts,
and manifests a job is writing live under `<data-location>/bergsmith/`, where
the data location is the table's `write.data.path` or
`<table-location>/data`. No object whose path below the table location contains
a `bergsmith` segment is deleted, which covers the current data location, the
`<table-location>/bergsmith/` layout of older releases, and any data location a
table used before `write.data.path` changed. Files an unresolved job may
publish are therefore protected twice: their outputs are under a Bergsmith
segment, and the deletion pass reloads table metadata so a commit that landed
protects its files as reachable.

**An empty table can be cleaned too.** Selection needs the table's locations
and metadata file, not a `main` snapshot. A table without a snapshot simply has
no snapshot-reachable files, and abandoned objects still become candidates.

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

**Deletion re-verifies before it removes.** A batch first does every read
(catalog load, reachability, listing), then fences the destructive step: the
coordinator proves it still holds the leader lease, the job is still
`Deleting`, and no cancellation was recorded. The fence is as late as possible,
and the receipts commit in a transaction that checks the lease again. The batch
uses the fresh listing's modification time and size rather than planning-time
values. A candidate is deleted only while it is still unreferenced, still older
than the effective window, still the listed object (same size), still outside
the Bergsmith prefix, and still inside the table location. Anything else is
recorded as skipped, never deleted. Deletes are idempotent, so a leader that
dies between the delete and the receipt only repeats itself.

**Ledger receipts make the job resumable and auditable.**
`orphan_deletions` records one row per planned candidate with its disposition
(`Deleted`, `Skipped`, or `Absent`), size, and time. `Deleted` means this batch
removed the object; `Skipped` means re-verification kept it; `Absent` means the
object was already gone, so an earlier attempt may have removed it but this
receipt cannot prove that it did — the reclaimed-byte total counts only
`Deleted`. Completion requires a receipt for
every planned candidate; the job then moves to `Succeeded`. A pass is bounded
by `orphans.max_deletions_per_pass`, so a large plan continues over several
passes and a restart resumes where the receipts stop.

**Deletion is off by default.** `orphans.enabled` must be set: the submission
API answers 400 while it is false, and planning and the first deletion fail
closed if it was disabled between them. Listing is bounded while it is
consumed (`orphans.max_listed_objects`), and the scan bounds manifests, manifest
entries, and object bytes (`orphans.max_scan_entries`,
`orphans.max_scan_bytes`). A manifest whose declared entry counts are missing
cannot be bounded, so the scan refuses it. Before a manifest is decoded its
object is stat-ed, so an understated length in the manifest list cannot bypass
`orphans.max_scan_bytes`. A compressed manifest could still decode to more
entries than the declared counts; the post-decode count check is the backstop. Exceeding any
bound fails the job permanently with a typed reason instead of deleting from a
partial view. A permanent failure
during deletion ends the job `Failed` through the deletion path, so it does not
repeat every pass.

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
- `orphans.max_scan_bytes` bounds a manifest list or manifest read.
- `POST /api/v1/jobs` accepts `operation: "remove_orphan_files"` while
  `orphans.enabled` is set; the table page offers the action, the job page
  labels it, and the maintenance history reports the operation so the
  publication lane never claims a commit for cleanup.
