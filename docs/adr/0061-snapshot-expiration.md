# 0061. Snapshot expiration with ledger-aware retention bounds

- Status: Proposed
- Date: 2026-10-27
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-031

## Context

Every commit adds a snapshot. A table maintained hourly accumulates thousands
per year, which grows table metadata, slows planning and readers, and retains
files that nothing needs. Iceberg's standard `RemoveSnapshots` update removes
snapshot entries from the table metadata.

Bergsmith cannot apply the catalog's default policy alone. A snapshot is still
needed when a nonterminal job planned against it, when a publication outcome is
unknown, or when the table has a branch or tag that names it. The connection
settings also promise protection windows: `max_execution_seconds` bounds a job's
run, and `max_unresolved_publication_seconds` bounds reconciliation. Source data
and delete files must stay readable through execution (plan §3), and an unknown
outcome must keep its evidence until it is resolved.

Removing a snapshot entry does not remove files. Data, manifest, and manifest
list files that no retained snapshot references become orphan candidates. That
is deliberate: file deletion needs its own reachability proof and is T2/T3's
work, not expiration's.

## Decision

**Expiration is metadata-only.** A publication carries only the standard
`remove-snapshots` update. It never deletes data, manifest, or manifest-list
files, and it never removes a snapshot reference in the first release of this
operation. Physical cleanup stays with BS-031.T2/T3.

**Catalog reference safety is a precondition, not an assumption.** A standard
REST catalog can accept `remove-snapshots` after a client read, remove a
snapshot that a concurrently created branch or tag references, and drop that
ref with it; no requirement set available to the client asserts the absence of
unknown refs. Expiration is therefore gated by
`expiration.catalog_ref_safety`, off by default. An operator may enable it only
after verifying that the catalog rejects the removal of any referenced
snapshot atomically with the commit. The submission API, the planner, and
every first expiration send fail closed when it is off. Evidence-based
recovery of an already authorized send stays available.

**Selection takes the stricter of table policy and Bergsmith's floor.**

- The effective maximum age is
  `max(history.expire.max-snapshot-age-ms, protection_window)`, where
  `protection_window` is the table's connection
  `max_execution_seconds + max_unresolved_publication_seconds`. A policy that
  is more conservative wins.
- The effective minimum is
  `max(history.expire.min-snapshots-to-keep, configured_minimum)`, at least
  one, applied along each retained branch's ancestry. The first release fixes
  `configured_minimum` at one; the table property raises it.
- The current snapshot, every branch head, and every tag head are never
  removed. Per-ref retention overrides for a branch (minimum and maximum age)
  apply; tag heads are kept without ancestry retention.
- Any snapshot newer than the effective cutoff is retained even when no ref
  reaches it, matching Java's `unreferencedSnapshotsToRetain`.
- Snapshot identifiers protected by the ledger are retained regardless of age:
  the planning baseline of every nonterminal job on the table and every
  snapshot held by an unresolved publication intent. Resolved intents pin
  nothing. Plan creation and expiration preparation share a table-scoped
  advisory lock, and the final pin query runs in the same transaction as the
  intent insert. Plan creation also rejects a baseline that intersects a
  pending expiration's removal set, so a delayed planner cannot pin an
  expiring snapshot after the intent commits.
- Output is deterministic: snapshot identifiers are sorted, and the same
  metadata, policy, bounds, and protected set produce the same plan.

**Execution uses the existing maintenance path.** Expiration is a job
operation (`ExpireSnapshots`) with a metadata-only plan: no worker tasks, no
rewrite. The first release exposes it through manual submission; a schedule
policy that carries retention parameters is left to a later task.
The coordinator acquires the table publication barrier, stores the immutable
baseline, protected set, cutoff, and exact update list, sends the commit under
the same send-authorization transaction as a rewrite, and records the result in
the ledger. Workers never see the job.

**A metadata commit's ambiguity is resolved by state, not by absence alone.**
There is no proposed snapshot to find. After an ambiguous send, reload the
table and require all three:

1. the `main` snapshot still equals the baseline snapshot, so no other write
   changed the head;
2. every snapshot named in the intent is absent;
3. every protected snapshot is present.

When all hold, the intent is `Succeeded`; when they do not, the outcome stays
unknown, the barrier stays held, and the ordinary attestation path resolves it.
The commit is never replayed automatically: `remove-snapshots` has no
server-side idempotency guarantee.

**Requirements are the baseline's.** The commit sends the original table UUID,
`main` ref snapshot, schema, partition spec, and sort order. Only the update
list differs from a rewrite commit, so the catalog's standard conditional
commit remains the authority.

## Consequences

- Expiration cannot break a running job, an unresolved publication, a branch,
  or a tag: their snapshots are excluded by the ledger and ref rules.
- Expiring snapshots leaves files behind. Storage grows until T2/T3 removes
  unreferenced files, and the storage breakdown (T4) shows the difference.
- Operators get a safe metadata operation before any file deletion exists,
  which is the order the plan's retention rules require.
- A metadata commit that is ambiguous is more expensive to resolve than a
  rewrite commit: absence is only accepted with an unchanged head and intact
  protected snapshots, and otherwise needs an operator's attestation.
- The operation needs a job/policy operation kind and a metadata publication
  intent; it is not a new worker path.

## Implementation

- The selection library, immutable `ExpirationPlanDocument`, and their tests
  live in `bergsmith-maintenance`.
- Migration 0036 adds `jobs.operation` and the immutable `expiration_plans`
  table. A metadata-only plan moves `Planning` directly to `ReadyToCommit`
  without a dispatch registration.
- `POST /api/v1/jobs` accepts `operation: "expire_snapshots"`; job documents
  expose it and retries copy it. The table page offers the action.
- Publication re-reads the ledger pins before writing the intent, sends the
  exact `remove-snapshots` request once, and recovers an ambiguous outcome
  through evidence or operator attestation. Replanning is required when a
  pinned snapshot appears after planning.
- Schedules and policy parameters for expiration, and preview counts, remain
  later work; no policy can select the operation yet.
