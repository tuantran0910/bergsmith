# Packaged observability

Prometheus alert rules and Grafana dashboards for the coordinator's metrics
listener. The decision is [ADR 0066](../../docs/adr/0066-packaged-alerts-and-dashboards.md);
the metric contract is [docs/operations/observability.md](../../docs/operations/observability.md).

|Path|Contents|
|---|---|
|`prometheus/rules/bergsmith.yaml`|Ten alert templates in evidence, work, and table groups|
|`prometheus/rules/bergsmith.test.yaml`|Twenty `promtool` behavioral scenarios|
|`prometheus/prometheus.example.yaml`|Operator scrape and rule configuration|
|`prometheus/acceptance/prometheus.yaml`|Acceptance scrape of the synthesized exposition|
|`grafana/dashboards/fleet.json`|Installation-wide fleet dashboard|
|`grafana/dashboards/connections.json`|Per-connection dashboard|
|`grafana/provisioning/`|Example datasource and dashboard providers|
|`compose.yaml`|Pinned Prometheus, Grafana, and fixture stack used by acceptance|

## Installing

1. Enable the listener in the coordinator configuration:

   ```json
   "observability": {
     "metrics_bind": "0.0.0.0:9464",
     "ledger_refresh_seconds": 15,
     "per_table_metrics": false
   }
   ```

   The listener has no authentication. Bind it to a network only the collector
   reaches. A standby coordinator serves neither probes nor metrics, so scrape
   the leader only; update the target on failover.

2. Copy `prometheus/rules/bergsmith.yaml` to the collector and reference it from
   `rule_files`. Start from `prometheus/prometheus.example.yaml` for the scrape
   job.

3. Tune the thresholds in your copy. Defaults assume a five-minute commit
   cadence and a 15-second metrics refresh:

   |Signal|Default|Meaning|
   |---|---|---|
   |Evidence age|120 s|A snapshot older than this is not current evidence|
   |Unresolved publication|3,600 s|Age of the oldest authorized, unresolved publication|
   |Schedule lag|300 s|The most overdue enabled schedule past its due instant|
   |Pending task without workers|300 s|Queued work with no live worker session|
   |Expired attempt leases|any for 10 m|Attempts holding expired leases and not yet fenced|
   |Critical tables|any for 15 m|Current health reports at least one critical table|

   Evidence alerts fire on their own component's `up`/age gauges. Dependent
   alerts include `and on()` gates on a successful read and a snapshot age under
   120 seconds, so a stale or failed cache suppresses the alerts that would
   otherwise interpret it. Grafana needs the same freshness on the panels; they
   show both component refreshes and their ages.

4. Import the two dashboards, or provision the directory and map the
   `bergsmith-prometheus` datasource UID to your Prometheus instance. Both
   dashboards are aggregate-only by default. The Connections variable comes
   from connection label values and needs no metric change.

5. Route alerts in Alertmanager. The rules set `severity: page` or
   `severity: ticket` and `component: coordinator` or `worker`; routing is
   operator-provided.

## Acceptance

`make observability-rules` runs `promtool check rules` and the behavioral
suite. `make observability` also renders the dashboards:

1. A PostgreSQL-backed coordinator test writes the exposition for four
   connections and four health states, with opt-in table series disabled, to
   `target/observability/fixture/metrics`.
2. The pinned `prom/prometheus:v3.15.1` scrapes that fixture and evaluates the
   shipped rules; the pinned `grafana/grafana:13.3.0` provisions the datasource
   and both dashboards.
3. `scripts/observability-grafana.mjs` substitutes the four scraped connection
   IDs into every panel target, executes each through Grafana's query API, and
   fails on any query or page error. It renders both dashboards in light and
   dark with the web gate's Chromium into `target/observability/screenshots/`.

Screenshots and the fixture are local artifacts, never source files.
