# 0066. Packaged alert rules and Grafana dashboards

- Status: Proposed
- Date: 2026-10-29
- Plan reference: `.private/BERGSMITH_PROJECT_PLAN.md` BS-030.T2

## Context

ADR 0044 serves ledger, publication, worker, and scheduler gauges from a
separate listener; ADR 0059 defines table health; ADR 0065 adds fleet health
and statistics. Operators still had to write every alert and dashboard
themselves, with two easy mistakes: alerting on a retained snapshot as if it
were fresh evidence, and summing per-connection totals that are deliberately
not additive.

The metrics contract is already closed: fixed state and status labels,
connection UUIDs, opt-in table UUIDs, and no free-form text. Packaging must not
add metrics, recording rules, or labels, because each would enlarge the
contract this decision would then have to maintain.

## Decision

Ship one Prometheus rule file and two Grafana dashboards under
`deploy/observability/`.

The rule file contains ten alert templates in three groups: evidence, work,
and tables. In the evidence group, two rules alert on refresh failure, two on
snapshot age while the component still reports success, and one on the absence
of any scraped ledger metric. The absence rule is the only one that covers a
dead or unscrapeable listener; every other rule needs a scrape to exist. Each
dependent rule is gated with `and on()` on a successful, recent read of the
snapshot it interprets, so one broken cache raises one evidence alert rather
than a burst of misleading work alerts. Work rules use only ledger metrics;
the table rule uses only health metrics. No rule uses a recording rule or a
new series.

Default thresholds are fixed expressions, tuned by editing an operator copy:
publication age 3,600 seconds, schedule lag 300 seconds, oldest pending task
300 seconds (only while no worker session is live), evidence freshness 120
seconds, critical tables for 15 minutes, and an absent scrape for 10 minutes.
Hold times are at least two evaluation intervals. Unresolved leases and
critical tables are tickets, not pages.

The Fleet dashboard shows evidence freshness first, then health counts and
assessed statistics, then queue, attempt, publication, session, schedule, and
RPC signals, and finally connection inventory. The Connections dashboard scopes
the same aggregates to one connection chosen from the
`bergsmith_connection_tables_health` label values. Both are aggregate-only
even when `per_table_metrics` is enabled: neither queries individual-table
series, and both state that statistics are logical sample sums and that
connection totals are not additive.

Panels use one datasource UID, `bergsmith-prometheus`, which operators map to
their Prometheus instance. The example scrape configuration targets the leader
only and keeps the shipped rules mounted; Alertmanager routing is
operator-provided, and Grafana provisioning is an example rather than a
deployment requirement.

## Consequences

Acceptance is reproducible without an operator environment: `promtool` checks
the rule file and runs twenty behavioral scenarios, and a PostgreSQL-backed
coordinator test renders the real exposition for four connections and all four
health states with individual table series disabled. Pinned Prometheus
`v3.15.1` and Grafana `13.3.0` scrape that exposition; a Node driver executes
every panel target through Grafana's query API and renders both dashboards in
light and dark with the web gate's pinned Chromium. Screenshots stay local
artifacts.

Operators must still choose thresholds, Alertmanager routes, and a scrape
target for their leader instance. Evidence age alerts require the metrics
listener to be enabled, and dependent rules stay quiet while a cache is stale
by design. OTLP export remains BS-030.T3, and the Helm packaging remains the
M8 milestone.
