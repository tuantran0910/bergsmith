// Visual baselines for the shell-heavy detail pages (BS-032.T8). They compare
// only in CI: the committed images were rendered by the pinned Linux browser,
// so a developer's platform fonts would otherwise report noise. Update them
// with `CI=1 npm --prefix web run e2e -- e2e/visual.spec.ts --update-snapshots`
// inside the Playwright container used by the release gate.

import { schema } from "../src/test/fakes";
import {
    CATALOG_ID,
    catalogs,
    catalogStorage,
    catalogTotals,
    managedCatalog,
    session,
    table,
} from "../src/test/fixtures";
import { type FakeCoordinator, expect, json, test } from "./coordinator";

function signedIn(coordinator: FakeCoordinator) {
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs))
        .on("GET", "/api/v1/catalogs/{id}", () => json(200, managedCatalog))
        .on("GET", "/api/v1/catalogs/{id}/storage", () =>
            json(200, catalogStorage),
        )
        .on("GET", "/api/v1/catalogs/{id}/statistics", () =>
            json(200, catalogTotals),
        )
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        );
}

test("catalog detail keeps its layout and tabs", async ({
    page,
    coordinator,
}, testInfo) => {
    test.skip(testInfo.project.name !== "light", "one baseline per page");
    await page.clock.install({ time: new Date("2026-10-29T12:00:00Z") });
    signedIn(coordinator);
    await page.setViewportSize({ width: 1280, height: 900 });
    await page.goto(`/catalogs/${CATALOG_ID}`);
    await expect(page.getByRole("heading", { name: "lake" })).toBeVisible();
    await expect(page.getByRole("tab", { name: "Overview" })).toBeVisible();
    await expect(page.getByText("Tables sampled", { exact: true })).toBeVisible();
    await expect(page).toHaveScreenshot("catalog-overview.png", {
        animations: "disabled",
        caret: "hide",
        fullPage: false,
    });

    await page.getByRole("tab", { name: "Settings" }).click();
    await expect(
        page.getByRole("heading", { name: "Connection settings" }),
    ).toBeVisible();
    await expect(page).toHaveScreenshot("catalog-settings.png", {
        animations: "disabled",
        caret: "hide",
        fullPage: false,
    });
});
