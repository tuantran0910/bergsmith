#!/usr/bin/env bash
# Dashboard acceptance for BS-030.T2: write the exposition fixture from the
# coordinator integration test, start the pinned Prometheus/Grafana stack,
# execute every panel query through Grafana, and render both dashboards in
# light and dark. The alert rules are checked by `make observability-rules`.
#
# Run through `make observability-dashboards`, which starts the test services,
# installs the web toolchain, and exports DATABASE_URL.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
compose=(docker compose --file "$root/deploy/observability/compose.yaml")
fixture="$root/target/observability/fixture"
screenshots="$root/target/observability/screenshots"

cleanup() {
	"${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

if ! docker info >/dev/null 2>&1; then
	echo "error: Docker is required for the observability acceptance stack" >&2
	exit 1
fi
: "${DATABASE_URL:?export the test database URL or run make observability-dashboards}"

rm -rf "$fixture" "$screenshots"
mkdir -p "$fixture" "$screenshots"
# Start from an empty TSDB: a previous run's series would otherwise satisfy the
# connection count and hide a broken scrape.
"${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true

# The exposition must come from the real metrics renderer, not a hand-written
# sample, so the dashboards are proven against the contract the coordinator
# serves.
echo "==> writing the exposition fixture from the coordinator test"
BERGSMITH_OBSERVABILITY_FIXTURE="$fixture/metrics" \
	cargo nextest run -p bergsmith-coordinator -E 'test(the_dashboard_fixture)' 
test -s "$fixture/metrics"

echo "==> starting Prometheus and Grafana"
"${compose[@]}" config >/dev/null
"${compose[@]}" up --detach

echo "==> executing and rendering the dashboards"
node "$root/scripts/observability-grafana.mjs"

echo "==> screenshots in target/observability/screenshots"
