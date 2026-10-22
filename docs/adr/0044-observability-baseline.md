# 0044. Observability baseline

- Status: Accepted
- Date: 2026-10-22
- Plan reference: BS-020.T2; ADR 0011 (execution ledger), ADR 0041 (serve runtime)

## Context

The first release needs to show pending work and scheduling latency, RPC
latency, running attempts and expired leases, retry causes and resource
failures, planned and rewritten bytes, retained outputs, commit conflicts and
unknown outcomes, cancellations, recovery age, and schedule lag. The serve
runtime logged plain text, had only a liveness probe, and no API showed worker
health.

Most of these facts already live in the PostgreSQL ledger as immutable or
monotonic rows. Process-local counters would reset on restart and disagree
after a leadership change.

## Decision

- **Ledger metrics are queries, not counters.** The leader serves Prometheus
  text at `/metrics` on an optional, separate listener
  (`observability.metrics_bind`), never on the browser origin. Each refresh
  reads one snapshot in a read-only `REPEATABLE READ` transaction with a
  statement timeout. The snapshot is cached for `ledger_refresh_seconds`
  behind a lock, so concurrent scrapers share one read. Counters are exposed
  only for monotonic facts: terminal jobs, attempt failures, started
  attempts, planned bytes, cancellation requests, and coalesced occurrences.
  Values that move between labels, such as output bytes by job outcome, are
  gauges.
- **RPC latency is process-local.** A tower layer on the worker gRPC server
  records a histogram by method and gRPC code and opens a `worker_rpc` span.
  Method labels come from a fixed list, so arbitrary paths cannot create
  series.
- **The exposition format is written in-tree.** The text format needs a few
  dozen lines with escaping tests; no metrics crate is added.
- **Readiness checks PostgreSQL.** `/readyz` returns `503` unless a trivial
  query answers within two seconds; `/healthz` stays liveness only.
- **Structured logs carry span context.** `BERGSMITH_LOG_FORMAT=json` selects
  JSON lines with the current span and its parents. Planning and publication
  run each job inside a span naming `job_id`. Spans are the trace baseline; an
  OpenTelemetry exporter is deferred to production work.
- **Worker health is an API resource.** `GET /api/v1/workers` lists current
  sessions with their status at database time and running attempt, for
  organization-wide `workers:manage`. No new permission is introduced.

## Consequences

- Metrics agree across restarts and failover, and a slow ledger delays a
  scrape by at most the statement timeout, at a refresh-interval granularity.
- Aggregates scan their tables. The refresh interval bounds the cost; very
  large ledgers may need summary tables later.
- A standby exposes no metrics until it leads, because no listener starts
  before leadership.
- Operators must keep the metrics listener on a private network.
