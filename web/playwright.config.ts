import { defineConfig, devices } from "@playwright/test";

import { ORIGIN } from "./e2e/coordinator";

// The browser gate (BS-021.T4). It runs the built UI from `dist/` against
// the fake coordinator in `e2e/coordinator.ts`; `make web-e2e` builds first.
export default defineConfig({
    testDir: "e2e",
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
