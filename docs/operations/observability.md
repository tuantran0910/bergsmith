# Observability

BS-020.T2 adds metrics, a readiness probe, structured logs, and a worker
health view. The decision is [ADR 0044](../adr/0044-observability-baseline.md).

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
  "ledger_query_timeout_millis": 5000
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
and gRPC codes; no credential, URL, or free-form text becomes a label.
`RequestTask` is a long poll, so its latency includes waiting for work.

Suggested alerts:

- `bergsmith_publications_unresolved > 0` for longer than your unresolved
  publication bound; see [recovery](recovery.md#unknown-publication-outcomes).
- `bergsmith_task_oldest_pending_seconds` above the job duration budget while
  `bergsmith_worker_sessions{status="live"}` is zero.
- `bergsmith_schedule_lag_seconds` above a few scheduler intervals.
- `bergsmith_ledger_up == 0`.

## Logs and traces

Both binaries log through `tracing`. `RUST_LOG` filters (default `info`), and
`BERGSMITH_LOG_FORMAT` selects `text` (default) or `json`. A JSON line carries
the event fields, the current span, and its parent spans; an unknown format
stops the process at startup. Spans name `job_id` for each job the planner
or publisher handles and the `method` of each worker RPC. Spans are the trace
baseline; there is no OpenTelemetry exporter yet. Credentials and tokens never
appear in fields.

## Worker health

`GET /api/v1/workers` lists the organization's current worker sessions: pool,
name, build, capabilities, advertised resources, status (`live`, `draining`,
or `expired`, at database time), lease expiry, and the attempt it runs. It
requires organization-wide `workers:manage` (administrators). The web UI shows
it on the Workers page.
