# 0064. Sampled table-prefix storage breakdown

- Status: Proposed
- Date: 2026-10-28
- Plan reference: BS-031.T4

## Context

Current statistics describe main's logical files. They cannot explain storage
retained by snapshots, metadata, or abandoned objects. Summing snapshot summaries
double-counts shared files; a live HTTP scan is expensive and can overload storage.

## Decision

A separate leader-owned, read-only collector captures complete observations.
It lists the table location and scans metadata and manifests only, never rows or
delete contents. HTTP reads PostgreSQL only. Collection is disabled by default,
independent of the orphan-deletion switch, and runs hourly when enabled.

Each normalized object identity (backend, authority, key) appears once in the
listed footprint, including aliases such as s3/s3a. Listed sizes determine bytes.
Conflicting duplicate sizes/timestamps fail collection. Categories are disjoint:

1. Metadata: reachable current/log metadata files, manifest lists/manifests, and
   statistics files named by current metadata.
2. Live: data/delete files in live manifest entries of current main.
3. Retained: live data/delete files of every other retained snapshot, excluding
   main's files. Branch/tag heads count here; retained does not mean obsolete.
4. Orphan candidates: remaining objects that satisfy ADR 0062's exact eligibility
   predicate, without the deletion plan's candidate cap. Age is the larger of
   orphan minimum age and the connection execution/unresolved-publication window.
5. Protected/other: the remainder, including Bergsmith trees, young/unknown-age
   objects, unresolved intent objects and conservative deleted-entry references.

The first five categories sum to the listing total. Referenced live data/delete
files outside the table root are separate: their known sizes come from manifests
and are excluded from totals. Custom external data paths therefore do not imply
zero data usage. This is a table-prefix observation, not a physical bucket bill.
Catalog rollups sum distinct visible table identities; shared files and overlapping
locations can count more than once. They report assessed/fresh/failed coverage
and oldest/newest sample times rather than one misleading freshness timestamp.

The existing bounded reachability scanner gains an optional accounting view.
Its conservative protection union is unchanged. Main is scanned first, and each
manifest/list is decoded once. Deleted entries remain cleanup-protected but never
become live accounting entries. No reporting result authorizes deletion.

Read ledger protections before catalog metadata. Re-load table UUID and metadata
location after scanning; discard an observation if the baseline changed. A failed
post-scan catalog read records `StorageBaselineUnavailable` and ends collection
for that pass. Alias fallback applies only to initial binding failures, so a
failed re-check cannot repeat a complete listing/scan through another alias.
This cannot make listing atomic against external writers or uploads; the UI
says so.
Failures preserve the last complete result and record a separate checked-at and
stable `Storage*` reason. List-count limits, cumulative read budgets,
inconsistent listing/metadata and accounting overflow have distinct typed
failures. Healthy observations become stale after two collection intervals,
allowing one interval of scheduling grace; failed checks are stale immediately.
No result means unassessed, never measured zero.

Bounds are independent reporting settings: 1 concurrent scan by default (up to
16), 20 tables per pass, one scan per catalog at a time; 1 million listed objects,
10,000 distinct lists, 100,000 manifests, 500,000 decoded entries, 64 MiB per
manifest/list and 1 GiB cumulative reads. Individual metadata reads time out
after 120 seconds; the complete listing and scan share a 600-second active
deadline, started after the per-catalog queue lock is acquired. Queueing does
not consume a table’s scan budget. Bounds fail closed, never publish partial
totals.
Reporting decodes use a collector-owned semaphore sized to
`max_concurrent_scans`. Its permit stays held until the blocking task exits,
even after a waiting scan times out. Cleanup decoding does not acquire this
reporting semaphore and retains its existing concurrency.
The same checked-at interval spaces failures; unchanged metadata is still scanned.
Ledger writes are leadership-fenced; no transaction spans I/O.

GET /api/v1/tables/{id}/storage and /api/v1/catalogs/{id}/storage follow inventory
visibility, return unknown/invisible resources as 404, and expose no object paths,
partition values, credentials, or third-party errors. Counters are decimal strings.

## Consequences

- Table and catalog pages explain footprint without request-time storage work.
- Listing is more expensive than main-only statistics, hence separate enablement,
  interval and budgets. Oversized tables stay explicitly unassessed/stale.
- Only latest complete samples and latest checks are stored; history charts,
  globally deduplicated bucket totals and automatic cleanup remain out of scope.
- Reporting does not certify a catalog/storage combination or broaden cleanup's
  safety claims in ADR 0062.
