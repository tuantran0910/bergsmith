# 0057. Metadata-only table statistics

- Status: Proposed
- Date: 2026-10-25
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` §4, BS-025

## Context

Planning already establishes that Bergsmith never reads table rows for
decisions, and BS-025 needs table size, file layout, delete pressure, and
history to show operators what maintenance has to work with. The numbers also
feed the health model and the table explorer (BS-026–BS-028), so they must be
consistent across pages and independent of any one job.

A table's current state is fully described by its metadata and manifests. The
data files themselves are only needed to answer row-level questions, which
statistics never ask.

## Decision

**A sample is read from metadata and manifests only.** `TableSample::collect`
reads the current snapshot's manifest list and every manifest entry, and never
opens a data or delete file. Counters come from live manifest entries rather
than the snapshot summary, because entries are authoritative, they carry the
delete kind, and the walk is required for the size histogram anyway. The
summary contributes only the last commit's operation. A table whose live
entries exceed `statistics.sample.max_manifest_entries` is skipped, exactly as
planning refuses it, rather than producing a partial sample.

**Samples are append-only and keyed by observation time.** `table_statistics`
stores one row per sample with a `(organization_id, table_identity_id,
sampled_at)` primary key. There is no update path: a correction is a new row.

**An unchanged metadata location records nothing.** The collector reads the
newest sample for the table and stops when its `metadata_location` matches the
loaded one, because every counter would be identical. Values that depend on
time — snapshot ages, the age of the last commit — are derived when the API
answers, from timestamps stored in the sample. History therefore grows with
commit rate, not with the sampling interval, and retention prunes it by age
(default 90 days).

**Sampling is bounded and ordered.** One pass samples at most
`statistics.page_size` due tables, oldest sample first, and processes each
catalog connection's tables in turn, so a connection sees one catalog request
at a time while connections progress independently. A failure scoped to one
table is logged and leaves that table's previous sample authoritative. A
confirmed publication requests an immediate sample for its table without
blocking the publication pass; the interval covers a dropped request.

**Counts and byte totals cross the API as decimal strings.** A browser parses
JSON numbers as doubles, so every counter, byte total, histogram bucket, and
partition count is a decimal string (`JobStatistics` sets this precedent).
Timestamps are RFC 3339 in UTC. Histogram buckets are fixed, ordered, and
include the open-ended final bucket, so two samples of one state serialize
identically.

**Partition values need maintenance access.** Counts, sizes, and the partition
count are inventory data. Partition values are table metadata that Bergsmith
otherwise exposes only to a principal allowed to plan (previews show the same
values), so they are omitted unless the caller holds `jobs:submit` on the
table.

**Aggregates respect grants.** Catalog and organization totals sum the newest
sample of every table the caller may see, using the same grant predicate as the
inventory list. A table-scoped grant does not reveal its connection, so the
catalog aggregate stays not-found for that caller while the organization
aggregate counts the table.

## Consequences

- Statistics stay cheap and safe on large tables: work is proportional to live
  manifest entries, not to rows, and no data file is ever fetched.
- A table above the entry bound has no statistics until an operator raises the
  bound; the API returns `null`, and pages show an explicit empty state.
- The histogram and partition labels are limited to what manifest entries
  record. Column metrics, row-group layout, and per-partition delete counts
  stay out of scope until a later work package needs them.
- BS-026 (explorer), BS-027 (impact), BS-028 (health), and BS-031 (broader
  maintenance) read these samples instead of adding their own collectors.
