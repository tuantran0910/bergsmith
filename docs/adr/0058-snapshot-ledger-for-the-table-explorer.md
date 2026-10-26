# 0058. Snapshot ledger for the table explorer

- Status: Proposed
- Date: 2026-10-26
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-026.T1

## Context

The table explorer's first task shows a table's write history: what each
commit did, how much it moved, which engine made it, and which Bergsmith job
published it. A statistics sample (ADR 0057) answers "what does the table look
like now"; it deliberately holds one row per observation, so it cannot answer
"what happened at 14:05".

Iceberg's metadata already records the whole answer. Each snapshot carries its
operation, its parent, its sequence number, its commit timestamp, and a summary
map with the engine's own change counters. Bergsmith's commit adapter writes
`bergsmith.job-id`, `bergsmith.publication-id`, and `bergsmith.proposal-sha256`
into that map (ADR 0015), so a commit made by Bergsmith names its job without a
join. Nothing in PostgreSQL durably links a published snapshot to a job: the
snapshot ID lives only inside the immutable commit intent's JSON, and `jobs`
carries no snapshot column.

The metadata file is not a stable store either. A table's snapshot list is
pruned by its own `history.expire` settings, and a catalog may drop the file.
Keeping the timeline in the metadata alone would make the explorer's history
as short as someone else's retention policy.

## Decision

**A dedicated append-only ledger stores one row per commit.** `table_snapshots`
is keyed `(organization_id, table_identity_id, snapshot_id)`: snapshot ID,
parent, sequence number, commit time, operation, and the summary map verbatim
as `jsonb`, plus the observation time. There is no update path; a correction is
a row the engine published under a new ID.

**The statistics sampler writes it.** The same pass that takes a sample reads
`TableSnapshot::collect`'s already-loaded metadata, so the ledger adds no
catalog request and never opens a data or delete file. One pass records at most
`statistics.sample.snapshot_history` snapshots (default 500, newest first) and
skips the ones already present with `ON CONFLICT DO NOTHING`. Collection sorts
borrowed snapshots before cloning a summary, so the cloned summaries are
bounded by the configured window; the reference vector and the sort still
scale with the table's retained snapshots.

**The sample and its commits are one write.** They commit in one transaction:
a failed ledger insert cannot leave a sample behind that the next pass reads as
unchanged. A table whose metadata is unchanged is additionally reconciled by
asking whether the table's *current* snapshot is recorded, not whether it is
the newest row. A rollback legitimately leaves newer commits in the ledger,
two commits can share an instant, and a snapshot ID does not encode commit
order, so an ordering comparison would rewrite the bounded window on every
pass after a rollback. The repair applies only when the current snapshot
belongs to the bounded window; outside it, absence is retention rather than a
gap.

**Retention follows the samples.** Rows are pruned by `observed_at` with the
same `statistics.retention_days` (default 90) and batch size as the samples:
the ledger keeps knowledge of a commit for as long as it keeps the samples
around it. Pruning never keys on `committed_at`, so a commit recorded late is
not lost immediately.

**The timeline is an inventory read; the cadence is derived, not stored.**
`GET /api/v1/tables/{id}/snapshots` lists recorded commits newest first with
the same cursor rules as every other list, and includes a commit-per-hour
bucket list for a window of 1 to 168 hours ending with the current hour. A
bucket is computed from `committed_at`; hours without a commit are present with
zero, so a chart never has to infer gaps. Counts and IDs cross the API as
decimal strings, timestamps as RFC 3339 in UTC.

**Deltas are parsed from the summary, not stored separately.** The standard
Iceberg keys (`added-data-files`, `deleted-data-files`, `added-records`,
`deleted-records`, `added-files-size`, `removed-files-size`, and the delete-file
counters) become the API's `delta` object; a key the engine did not write counts
as zero. The rest of the map is exposed as-is, so an operator sees the engine
that committed and any evidence it recorded — except the per-partition counters
Iceberg writes under `partitions.<partition-path>`, whose keys embed the
partition value itself. Those follow the statistics rule (ADR 0057) and are
filtered out unless the caller may submit jobs on the table. Inventory
visibility decides the rest of the document, including Bergsmith's job link.

## Consequences

- History survives snapshot expiry in the catalog, up to the ledger's
  retention, and it costs one row per commit rather than a copy of the snapshot
  list per sample.
- A foreign engine's commits appear without any cooperation; only the job link
  depends on Bergsmith's summary properties.
- Cadence counts recorded commits. A burst older than the 500-snapshot bound or
  the retention window is absent from both the timeline and the chart, which is
  honest for "how busy is this table now" and wrong for billing.
- A table above the bound gets the newest 500 commits on its first pass; older
  history is not backfilled, because the bound is what keeps one pass cheap.
- Every unchanged pass checks whether the table's current snapshot is recorded:
  one primary-key probe per table, and a write of the bounded window when it is
  missing and belongs to that window. A commit missing from the middle of the
  ledger while the current snapshot is present is not detected; the
  transactional write is what prevents that state.
- A table rolled back to an older snapshot, or whose commits share an instant,
  records nothing on a pass: membership settles it, and the current snapshot
  stays in the ledger until retention removes it with the rest of the view.
- The ledger duplicates what the metadata holds while the metadata still holds
  it. If that becomes material for very hot tables, the bound and retention are
  the two knobs, both configuration.
- BS-027 (impact), BS-028 (health), and BS-031 (broader maintenance) can read
  commit history from this ledger instead of re-walking metadata.
