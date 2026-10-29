import { defineConfig, devices } from "@playwright/test";

import { ORIGIN } from "./e2e/coordinator";

// The browser gate (BS-021.T4). It runs the built UI from `dist/` against
// the fake coordinator in `e2e/coordinator.ts`; `make web-e2e` builds first.
export default defineConfig({
    testDir: "e2e",
    // Baselines are rendered by the pinned Linux browser; other platforms
    // would compare against their own fonts and report noise.
    testIgnore: process.platform === "linux" ? [] : ["**/visual.spec.ts"],
    // Visual baselines are rendered by the pinned Linux browser in CI; a
    // developer's platform fonts would otherwise report noise. See
    // e2e/visual.spec.ts for the update command.
    snapshotPathTemplate:
        "{testDir}/{testFilePath}-snapshots/{arg}{-projectName}{ext}",
    ignoreSnapshots: !process.env["CI"],
    fullyParallel: true,
    forbidOnly: !!process.env["CI"],
    retries: 0,
    reporter: process.env["CI"]
        ? [["list"], ["html", { open: "never" }]]
        : "list",
    use: {
        ...devices["Desktop Chrome"],
        baseURL: ORIGIN,
        locale: "en-US",
        timezoneId: "UTC",
        trace: "retain-on-failure",
    },
    // The theme follows the system, so each journey runs in both themes and
    // axe checks contrast in each.
    projects: [
        { name: "light", use: { colorScheme: "light" } },
        { name: "dark", use: { colorScheme: "dark" } },
    ],
});
