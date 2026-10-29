// Responsive card layout at phone widths (BS-032.T4): every primary list
// renders cards instead of a wide grid, the page never scrolls horizontally,
// and no visible action is clipped at 320 or 390 pixels. Screenshots are test
// artifacts for review, never committed baselines.

import type { Page } from "@playwright/test";

import { schema } from "../src/test/fakes";
import {
    catalogs,
    overview,
    policies,
    schedules,
    session,
    table,
} from "../src/test/fixtures";
import { type FakeCoordinator, expect, json, test } from "./coordinator";

/** The routes the primary lists read. */
function signedIn(coordinator: FakeCoordinator) {
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/overview", () => json(200, overview))
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs))
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        )
        .on("GET", "/api/v1/policies", () => json(200, policies))
        .on("GET", "/api/v1/schedules", () => json(200, schedules));
}

/** The document never grows wider than the viewport. */
async function expectNoHorizontalOverflow(page: Page) {
    const width = await page.evaluate(() => ({
        scroll: document.documentElement.scrollWidth,
        client: document.documentElement.clientWidth,
    }));
    expect(width.scroll, "document width").toBeLessThanOrEqual(width.client);
}

/** Every visible action in the page body is fully inside the viewport. */
async function expectActionsInsideViewport(page: Page) {
    const clipped = await page.evaluate(() =>
        [...document.querySelectorAll("main a, main button")]
            .filter((element) => {
                const style = getComputedStyle(element);
                if (style.display === "none" || style.visibility === "hidden") {
                    return false;
                }
                const rect = element.getBoundingClientRect();
                return (
                    rect.width > 0 &&
                    (rect.left < -0.5 || rect.right > window.innerWidth + 0.5)
                );
            })
            .map((element) => element.textContent.trim().slice(0, 40)),
    );
    expect(clipped, "clipped actions").toEqual([]);
}

for (const width of [320, 390] as const) {
    test.describe(`${width}px`, () => {
        test.use({ viewport: { width, height: 800 } });

        test("renders the primary lists as cards without overflow", async ({
            page,
            coordinator,
        }, testInfo) => {
            signedIn(coordinator);
            for (const [path, name] of [
                ["/tables", "tables"],
                ["/policies", "policies"],
                ["/schedules", "schedules"],
            ] as const) {
                await page.goto(path);
                await expect(
                    page.getByRole("region", {
                        name: /cards$/,
                    }),
                ).toBeVisible();
                await expect(page.locator("article").first()).toBeVisible();
                await expectNoHorizontalOverflow(page);
                await expectActionsInsideViewport(page);
                await page.screenshot({
                    path: testInfo.outputPath(`${width}-${name}.png`),
                    fullPage: true,
                });
            }
        });

        test("opens the page's filters in a drawer", async ({
            page,
            coordinator,
        }) => {
            signedIn(coordinator);
            await page.goto("/tables");
            await page.getByRole("button", { name: "Filters" }).click();
            const drawer = page.getByRole("dialog", { name: "Filters" });
            await expect(drawer).toBeVisible();
            await expect(drawer.getByText("Health")).toBeVisible();
            await drawer.getByRole("combobox", { name: "Health" }).click();
            await drawer.getByRole("option", { name: "Critical" }).click();
            await expect(
                drawer.getByRole("combobox", { name: "Health" }),
            ).toContainText("Critical");
            await expect(drawer.getByText(/of 1 tables/)).toBeVisible();
            await expect(
                drawer.getByRole("button", { name: "Show results" }),
            ).toBeVisible();
            await expectNoHorizontalOverflow(page);
            await page.getByRole("button", { name: "Show results" }).click();
            await expect(drawer).toBeHidden();
        });
    });
}
