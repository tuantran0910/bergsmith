# 0065. Bounded health and statistics metrics

- Status: Proposed
- Date: 2026-10-28
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` BS-030.T1

## Context

ADR 0044 supplies a cached, separate Prometheus listener. ADR 0059 defines
health using complete metadata samples and the worst enabled policy. Operators
need fleet health and statistics without exporting a series for every table by
default, or performing catalog requests during a scrape.

## Decision

Extend the existing refresh with current health counts and seven statistics
sums: data files, data bytes, delete files, delete bytes, records, manifests,
and retained snapshots. All are gauges. Emit global counts by health state and
counts and sums per connection. The health states are healthy, warning,
critical, and unassessed; emit zero series for each state and connection.

Evaluate with the same function as the table API, at the transaction's clock,
using confirmation freshness and enabled policies. No sample, a legacy sample,
or an unusable sample is unassessed. Exclude unassessed tables from statistics
sums, and expose their health count as coverage evidence. No catalog or object
storage request is involved. Statistics remain observations, not physical
storage totals; sample times can differ between tables.

Count each registered canonical identity once globally and once per connection
that names it, regardless of alias count. A shared table contributes to each
connection; connection sums are not additive across connections. Identities
without aliases are unregistered and excluded from health, statistics, and
individual series. Their historical samples cannot generate staleness alerts.
Disabled connections remain visible while their aliases are registered.

Keep individual table series off by default. `observability.per_table_metrics`
enables them, bounded by `observability.max_table_metrics` (default 1,000,
allowed 1 through 100,000). Select the lowest canonical UUIDs
deterministically. The cap bounds current exposition, not historical series
churn as inventory changes. Expose `bergsmith_table_metrics_omitted` for cap
exclusions; it is zero when individual metrics are disabled. The cap never
truncates aggregates.

Only connection UUIDs, opt-in table UUIDs, and fixed states become new labels.
No names, paths, URLs, policy names, partition values, or secret references
are exported. The existing network-isolated, unauthenticated listener exposes
installation-wide aggregates, not an individual principal's grant projection.

Refresh ledger and health/statistics metrics independently, in separate cached
read-only repeatable-read transactions. Ledger statements retain their
existing `ledger_query_timeout_millis` bound. The health scan has its own
statement and whole-scan deadline, `observability.health_query_timeout_millis`
(default 5,000; positive), also used to bound connection acquisition. Both
caches use `ledger_refresh_seconds`, and concurrent scrapers share each
refresh. A scrape starts both refreshes concurrently, so their independent
waits do not add up.

A failed health scan preserves only the previous complete health snapshot,
with `bergsmith_health_metrics_up 0` and
`bergsmith_health_metrics_snapshot_age_seconds`. It does not fail or roll back
the ledger refresh. Ledger failures continue to use `bergsmith_ledger_up` and
`bergsmith_ledger_snapshot_age_seconds`. The two components may describe
different database instants; each snapshot is internally consistent. A
component has no data families until its first successful refresh.

Traverse registered tables in pages of 256 ordered by organization and table
UUID, using the existing organization/table unique index. Batch sample, check,
and policy reads per organization in each page, using newest-row index
lookups. Organization ordering prevents interleaved table creation from
splitting one tenant into small batches on every page. A bounded ordered map
selects the lowest table UUIDs globally, independently of organization order.
Keep only aggregates and capped individual series between pages.

Unusable-sample and policy-decoding diagnostics are debug events, so recurring
metrics refreshes do not emit a warning for every affected table. Unassessed
coverage remains visible in metrics.

## Consequences

Default new series cardinality is 11 per connection plus 14 global series.
Opt-in table metrics add at most nine series per selected table: one numeric
health state, seven statistics gauges, and one confirmation timestamp. Missing
measurements are absent from individual statistics series, rather than zero.
The numeric health states are 0 healthy, 1 warning, 2 critical, 3 unassessed.

Refreshing evaluates each registered table and its enabled schedule
associations once; newest-sample reads seek the existing primary key rather
than scanning history. Database work and failures are visible through each
component's up/age metrics. Large installations can raise refresh and health
query bounds without extending ledger statement bounds. An individual-table
cap bounds exposition memory but does not skip evaluation of tables needed for
accurate aggregate health.

Alert rules and dashboards remain BS-030.T2; OTLP export remains BS-030.T3.