// Executes every dashboard panel query through Grafana and renders both
// dashboards in light and dark themes with the web UI's pinned Chromium.
//
// This is the BS-030.T2 acceptance driver. It reads the committed dashboard
// JSON (the same files Grafana provisions), substitutes the real connection
// IDs Prometheus scraped from the coordinator test fixture, and fails on any
// query or rendering error. Screenshots land in target/observability/ as
// developer artifacts, never source files.
//
// Environment:
//   GRAFANA_URL     default http://127.0.0.1:53000
//   PROMETHEUS_URL  default http://127.0.0.1:59090
//   OBSERVABILITY_SCREENSHOTS default target/observability/screenshots

import { mkdir, readdir, readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const grafana = process.env.GRAFANA_URL ?? "http://127.0.0.1:53000";
const prometheus = process.env.PROMETHEUS_URL ?? "http://127.0.0.1:59090";
const screenshots =
  process.env.OBSERVABILITY_SCREENSHOTS ?? join(root, "target/observability/screenshots");
const dashboardsDirectory = join(root, "deploy/observability/grafana/dashboards");
const datasource = { type: "prometheus", uid: "bergsmith-prometheus" };
const FIXTURE_CONNECTIONS = 4;

const require = createRequire(join(root, "web/package.json"));
const { chromium } = require("playwright");

function fail(message) {
  throw new Error(message);
}

async function json(url, init) {
  const response = await fetch(url, init);
  const body = await response.text();
  if (!response.ok) {
    fail(`${init?.method ?? "GET"} ${url} answered ${response.status}: ${body}`);
  }
  try {
    return JSON.parse(body);
  } catch {
    fail(`${url} did not answer JSON: ${body.slice(0, 200)}`);
  }
}

async function waitFor(label, probe, attempts = 120) {
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    try {
      if (await probe()) {
        return;
      }
    } catch {
      // Keep polling while the container starts.
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  fail(`${label} did not become ready`);
}

async function prometheusQuery(expression) {
  const data = await json(
    `${prometheus}/api/v1/query?query=${encodeURIComponent(expression)}`,
  );
  if (data.status !== "success") {
    fail(`Prometheus rejected ${expression}: ${JSON.stringify(data)}`);
  }
  return data.data.result;
}

async function dashboards() {
  const names = (await readdir(dashboardsDirectory)).filter((name) => name.endsWith(".json"));
  if (names.length === 0) {
    fail(`no dashboard JSON in ${dashboardsDirectory}`);
  }
  return Promise.all(
    names.map(async (name) => {
      const dashboard = JSON.parse(await readFile(join(dashboardsDirectory, name), "utf8"));
      if (!dashboard.uid || !Array.isArray(dashboard.panels)) {
        fail(`${name} is not a dashboard with a uid and panels`);
      }
      for (const panel of dashboard.panels) {
        for (const target of panel.targets ?? []) {
          if (typeof target.expr !== "string" || target.expr.trim() === "") {
            fail(`${name}: panel ${panel.title} has a target without an expression`);
          }
        }
      }
      return { name, dashboard };
    }),
  );
}

function connectionVariable(dashboard) {
  return (dashboard.templating?.list ?? []).find((variable) => variable.name === "connection");
}

async function executeTarget(expression, from, to) {
  const data = await json(`${grafana}/api/ds/query`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      queries: [
        {
          refId: "A",
          datasource,
          expr: expression,
          instant: false,
          range: true,
          intervalMs: 1_000,
          maxDataPoints: 200,
        },
      ],
      from: String(from),
      to: String(to),
    }),
  });
  const result = data.results?.A;
  if (!result) {
    fail(`Grafana returned no result for ${expression}: ${JSON.stringify(data)}`);
  }
  if (result.error) {
    fail(`Grafana failed ${expression}: ${result.error}`);
  }
  if (!Array.isArray(result.frames)) {
    fail(`Grafana returned no frames for ${expression}`);
  }
}

async function executeQueries(loaded, connections) {
  const now = Date.now();
  const from = now - 15 * 60_000;
  let executed = 0;
  const perConnection = new Map();

  for (const { name, dashboard } of loaded) {
    const scoped = connectionVariable(dashboard) !== undefined;
    for (const panel of dashboard.panels) {
      for (const target of panel.targets ?? []) {
        if (scoped) {
          for (const connection of connections) {
            await executeTarget(target.expr.replaceAll("$connection", connection), from, now);
            executed += 1;
            perConnection.set(connection, (perConnection.get(connection) ?? 0) + 1);
          }
        } else {
          await executeTarget(target.expr, from, now);
          executed += 1;
        }
      }
      process.stdout.write(".");
    }
    process.stdout.write(` ${name} queries executed\n`);
  }

  for (const connection of connections) {
    if (!perConnection.has(connection)) {
      fail(`connection ${connection} never reached a dashboard query`);
    }
  }
  return executed;
}

async function render(loaded) {
  await mkdir(screenshots, { recursive: true });
  const browser = await chromium.launch();
  const path = [];
  try {
    for (const theme of ["light", "dark"]) {
      const page = await browser.newPage({ colorScheme: theme, viewport: { width: 1600, height: 2600 } });
      const pageErrors = [];
      page.on("pageerror", (error) => pageErrors.push(error.message));
      for (const { dashboard } of loaded) {
        const url = `${grafana}/d/${dashboard.uid}?orgId=1&kiosk&theme=${theme}&from=now-15m&to=now&refresh=`;
        await page.goto(url, { waitUntil: "load" });
        await page.waitForSelector('[data-testid="data-testid panel content"]', { state: "attached" });
        const panels = await page.locator('[data-testid="data-testid panel content"]').count();
        if (panels === 0) {
          fail(`${dashboard.uid} rendered no panels`);
        }
        await page.waitForLoadState("networkidle");
        // Timeseries panels draw after the first query round; wait for a chart
        // canvas and then for the last paint.
        await page.waitForFunction(
          () => document.querySelectorAll('[data-testid="data-testid panel content"] canvas').length > 0,
          undefined,
          { timeout: 15_000 },
        );
        await page.waitForTimeout(3_000);
        const file = join(screenshots, `${dashboard.uid}-${theme}.png`);
        await page.screenshot({ path: file, fullPage: true });
        path.push(file);
      }
      if (pageErrors.length > 0) {
        fail(`the dashboards raised page errors: ${pageErrors.join("; ")}`);
      }
      await page.close();
    }
  } finally {
    await browser.close();
  }
  return path;
}

async function main() {
  await waitFor("Grafana", async () => (await fetch(`${grafana}/api/health`)).ok);
  await waitFor("Prometheus scrape", async () => {
    const target = await prometheusQuery('up{job="bergsmith"}');
    return target.length === 1 && target[0].value[1] === "1";
  });
  await waitFor("four fixture connections", async () => {
    const values = await json(
      `${prometheus}/api/v1/label/connection_id/values`,
    );
    return values.status === "success" && values.data.length === FIXTURE_CONNECTIONS;
  });
  const connections = (
    await json(`${prometheus}/api/v1/label/connection_id/values`)
  ).data;
  const loaded = await dashboards();
  const executed = await executeQueries(loaded, connections);
  const rendered = await render(loaded);

  for (const { name, dashboard } of loaded) {
    const scoped = connectionVariable(dashboard) !== undefined;
    console.log(
      `ok ${name}: ${dashboard.panels.length} panels` +
        (scoped ? ` across ${connections.length} connections` : ""),
    );
  }
  console.log(
    `executed ${executed} panel queries and wrote ${rendered.length} screenshots`,
  );
}

main().catch((error) => {
  console.error(`observability dashboards: ${error.message}`);
  process.exit(1);
});
