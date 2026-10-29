# Observability

BS-020.T2 adds metrics, a readiness probe, structured logs, and a worker
health view. The decision is [ADR
0044](../adr/0044-observability-baseline.md).

## Probes

| Path | Listener | Answers |
|---|---|---|
| `/healthz` | HTTP | `200` while the process serves HTTP. Use it for liveness |
| `/readyz` | HTTP | `200` when PostgreSQL answers within two seconds, otherwise `503`. Use it for readiness |

A standby coordinator waits for the leadership lease before it starts any
listener, so it answers neither probe until it leads.

## Metrics

Set `observability.metrics_bind` (for example `0.0.0.0:9464`) to serve
Prometheus text at `GET /metrics` on a separate listener. The listener has no
authentication; bind it to a network only your collector reaches. It is off by
default.

```json
"observability": {
  "metrics_bind": "0.0.0.0:9464",
  "ledger_refresh_seconds": 15,
  "ledger_query_timeout_millis": 5000,
  "health_query_timeout_millis": 5000,
  "per_table_metrics": false,
  "max_table_metrics": 1000
}
```

Ledger metrics are read from PostgreSQL in one read-only transaction with a
statement timeout, then reused for `ledger_refresh_seconds`, so several
scrapers cost one read per interval. They come from durable rows, so they are
continuous across restarts and leadership changes. When a read fails,
`bergsmith_ledger_up` is `0` and the last good values stay exposed with their
age; a failed read is retried after the refresh interval.

| Metric | Type | Meaning |
|---|---|---|
| `bergsmith_build_info{version}` | gauge | Coordinator build; always 1 |
| `bergsmith_ledger_up` | gauge | Whether the last ledger read succeeded |
| `bergsmith_ledger_snapshot_age_seconds` | gauge | Age of the values below |
| `bergsmith_jobs_active{state}` | gauge | Non-terminal jobs by state |
| `bergsmith_jobs_ended_total{state}` | counter | Jobs that reached `Succeeded`, `NoOp`, `Cancelled`, `ReplanRequired`, or `Failed` |
| `bergsmith_tasks_pending` | gauge | Tasks waiting for a worker |
| `bergsmith_task_oldest_pending_seconds` | gauge | Wait of the oldest pending task (scheduling latency) |
| `bergsmith_tasks_retry_waiting` | gauge | Tasks waiting for a retry timer |
| `bergsmith_attempts_running` | gauge | Granted or running attempts with a current lease |
| `bergsmith_attempts_lease_expired` | gauge | Attempts whose lease expired and that are not fenced yet |
| `bergsmith_attempts_started_total` | counter | Attempts started; each consumes retry budget |
| `bergsmith_attempt_failures_total{reason,disposition}` | counter | Attempt failures by stable reason code (for example `ProcessMemoryLimitExceeded`) and disposition |
| `bergsmith_planned_input_bytes_total` | counter | Input bytes of every persisted plan |
| `bergsmith_output_bytes{outcome}` | gauge | Accepted output bytes by job outcome: `published`, `conflict` (work discarded after a conflict), `abandoned`, `pending` |
| `bergsmith_retained_unused_output_prefixes` | gauge | Attempt output prefixes of ended jobs that no publication uses |
| `bergsmith_publications_unresolved` | gauge | Authorized publications without a recorded outcome, including `CommitOutcomeUnknown` |
| `bergsmith_publication_oldest_unresolved_seconds` | gauge | Age of the oldest unresolved publication |
| `bergsmith_cancellation_requests_total` | counter | Cancellation requests |
| `bergsmith_cancellation_requests_too_late_total` | counter | Cancellations requested after publication started |
| `bergsmith_schedule_occurrences{state}` | gauge | Schedule occurrences by state |
| `bergsmith_schedule_occurrences_coalesced_downtime_total` | counter | Occurrences that folded instants missed during downtime |
| `bergsmith_schedule_lag_seconds` | gauge | How far the most overdue enabled schedule is past its due instant |
| `bergsmith_worker_sessions{status}` | gauge | Current worker sessions: `live`, `draining`, `expired` |
| `bergsmith_worker_rpc_duration_seconds{method,code}` | histogram | Worker RPC latency on this coordinator, by method and gRPC code; resets on restart |

Labels are only states, stable reason codes, dispositions, RPC method names,
and gRPC codes, plus connection UUIDs and opt-in table UUIDs; no credential,
URL, or free-form text becomes a label. `RequestTask` is a long poll, so its
latency includes waiting for work.

### Health and table statistics

BS-030.T1 ([ADR 0065](../adr/0065-bounded-health-and-statistics-metrics.md))
adds an independently cached transaction with the following gauges. Health is
re-evaluated at database time on every refresh, using the API's worst enabled
policy and last metadata confirmation. No catalog or storage calls occur.

| Metric | Labels | Meaning |
|---|---|---|
| `bergsmith_health_metrics_up` | none | Whether the last health/statistics refresh succeeded |
| `bergsmith_health_metrics_snapshot_age_seconds` | none | Age of the health/statistics values |
| `bergsmith_tables_health` | `state` | Registered canonical tables by `healthy`, `warning`, `critical`, or `unassessed` |
| `bergsmith_connection_tables_health` | `connection_id`, `state` | Distinct tables named by each connection, by health |
| `bergsmith_statistics_<counter>` | none | Sum over newest complete samples of assessed tables |
| `bergsmith_connection_statistics_<counter>` | `connection_id` | Same sum over distinct tables named by the connection |
| `bergsmith_table_health` | `table_id` | Opt-in state: healthy=0, warning=1, critical=2, unassessed=3 |
| `bergsmith_table_statistics_<counter>` | `table_id` | Opt-in newest complete table counters; absent when unassessed |
| `bergsmith_table_statistics_confirmed_timestamp_seconds` | `table_id` | Opt-in last confirmation as Unix seconds; absent without a sample |
| `bergsmith_table_metrics_omitted` | none | Tables excluded by the individual-series cap; zero when disabled |

`<counter>` is one of `data_files`, `data_bytes`, `delete_files`,
`delete_bytes`, `records`, `manifests`, or `snapshots`. These are logical
metadata sample totals, not physical storage usage. Samples can have different
ages. Missing, legacy, or unusable evidence is unassessed; its counters do not
contribute to sums. Check unassessed counts before interpreting totals as
complete. Empty connections emit zeros. Disabled connections remain included.

Multiple aliases of one table in a connection count once. A table shared by
two connections counts in both; connection totals must not be summed to infer
global totals. Identities without aliases are unregistered and excluded from
all health/statistics metrics, even while historical samples remain.

Individual-table series are off by default. Enable `per_table_metrics` only
when needed. `max_table_metrics` (default 1,000, valid 1–100,000) limits them
to the lowest canonical table UUIDs, without limiting aggregate coverage. Each
selected table adds at most nine series. Inventory changes can select
different UUIDs and therefore create historical series churn. Default new
cardinality is eleven series per connection plus fourteen global series. The
listener exposes installation-wide data; restrict its network access.

Health refreshes have an independent statement and whole-scan deadline,
`health_query_timeout_millis` (default 5,000; must be positive). Ledger and
health caches share the refresh interval but can describe different database
instants. Each scrape starts both refreshes concurrently. A failed health
refresh keeps its previous complete values, sets `bergsmith_health_metrics_up`
to zero, and leaves ledger refreshes independent. Alert on that signal and
`bergsmith_health_metrics_snapshot_age_seconds` as well as health: retained
values cannot represent new freshness transitions. Ledger failures retain
their own snapshot and set `bergsmith_ledger_up` to zero. Neither component
exposes partial refresh results.

Registered identities are paged by organization and table UUID. Each page
batches reads per organization; the individual-series cap still selects the
lowest table UUIDs globally. Unusable-sample and policy-decoding diagnostics
are logged at debug level to avoid recurring per-table warnings.

### Alerts and dashboards

Prometheus alert rules and Grafana dashboards ship in
[`deploy/observability`](../../deploy/observability/README.md); the decision is
[ADR 0066](../adr/0066-packaged-alerts-and-dashboards.md). They read only the
metrics above and add no series.

Ten alert templates cover the ledger and health evidence, unresolved
publications, schedule lag, queued work without a live worker, expired leases,
and critical tables. Each component's refresh failure and evidence age alert
independently; every dependent rule requires a successful read and a snapshot
age under two minutes, so a broken cache raises one evidence alert instead of a
burst about stale work. Defaults are publication age 3,600 seconds, schedule
lag 300 seconds, and pending work 300 seconds; tune them with your commit
cadence and worker count.

The Fleet dashboard follows the same evidence-first order and shows health,
logical statistics, queue and publication depth, worker sessions, schedule lag,
and RPC latency. The Connections dashboard scopes those aggregates to one
connection. Both are aggregate-only and do not query individual-table series, even when
`per_table_metrics` is enabled; the dashboards state that connection totals are
not additive and that statistics are metadata sample sums rather than physical
storage. Missing evidence is visible as unassessed coverage, not as
healthy.

Scrape the leader only, as for the probes above, and route the alerts'
`severity` and `component` labels through your Alertmanager.

## Logs and traces

Both binaries log through `tracing`. `RUST_LOG` filters (default `info`), and
`BERGSMITH_LOG_FORMAT` selects `text` (default) or `json`. A JSON line carries
the event fields, the current span, and its parent spans; an unknown format
stops the process at startup. Spans name `job_id` for each job the planner or
publisher handles and the `method` of each worker RPC. Spans are the trace
baseline; there is no OpenTelemetry exporter yet. Credentials and tokens never
appear in fields.

## Worker health

`GET /api/v1/workers` lists the organization's current worker sessions: pool,
name, build, capabilities, advertised resources, status (`live`, `draining`,
or `expired`, at database time), lease expiry, and the attempt it runs. It
requires organization-wide `workers:manage` (administrators). The web UI shows
it on the Workers page.