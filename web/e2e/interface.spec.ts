// The interface system (BS-033.T1 to T3, ADR 0067): the font is served by
// the deployment's own origin and preloaded, the type scale sets 13 px body
// text, reduced motion leaves no transition behind an interaction, the
// floating primitives work under the CSP (the fixture fails on violations),
// the shell's rail collapses and remembers it, the command palette finds and
// opens without writing, and the live stream refreshes an open list (T8).

import { schema } from "../src/test/fakes";
import {
    CATALOG_ID,
    JOB_ID,
    TABLE_ID,
    catalogs,
    catalogStorage,
    catalogTotals,
    job,
    managedCatalog,
    session,
    table,
} from "../src/test/fixtures";
import {
    type FakeCoordinator,
    expect,
    expectAccessible,
    json,
    liveStream,
    test,
} from "./coordinator";

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

test("loads the interface font from its own origin", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    const fonts: string[] = [];
    page.on("request", (request) => {
        if (request.resourceType() === "font") {
            fonts.push(request.url());
        }
    });
    await page.goto(`/catalogs/${CATALOG_ID}`);
    await expect(page.getByRole("heading", { name: "lake" })).toBeVisible();

    const preload = page.locator('link[rel="preload"][as="font"]');
    await expect(preload).toHaveCount(1);
    await expect(preload).toHaveAttribute(
        "href",
        /^\/assets\/inter-latin-wght-normal-[\w-]+\.woff2$/,
    );
    await expect(preload).toHaveAttribute("crossorigin", "");

    // The page itself asked for the face: nothing here forces a load.
    expect(
        await page.evaluate(async () => {
            await document.fonts.ready;
            return [...document.fonts]
                .filter((face) => face.status === "loaded")
                .map((face) => face.family.replaceAll('"', ""));
        }),
    ).toContain("Inter Variable");
    const origin = new URL(page.url()).origin;
    expect(fonts.length).toBeGreaterThan(0);
    for (const font of fonts) {
        expect(new URL(font).origin).toBe(origin);
    }
    // The preload and the stylesheet share one download.
    expect(new Set(fonts).size).toBe(fonts.length);
});

test("sets body text on the type scale", async ({ page, coordinator }) => {
    signedIn(coordinator);
    await page.goto(`/catalogs/${CATALOG_ID}`);
    const body = await page.evaluate(() => {
        const style = getComputedStyle(document.body);
        return {
            size: style.fontSize,
            lineHeight: style.lineHeight,
            family: style.fontFamily,
        };
    });
    expect(body).toEqual({
        size: "13px",
        lineHeight: "20px",
        family: expect.stringMatching(/^"Inter Variable", "Inter Fallback"/),
    });
    const title = page.getByRole("heading", { level: 1, name: "lake" });
    await expect(title).toHaveCSS("font-size", "20px");
    await expect(title).toHaveCSS("font-weight", "600");
});

test("reduced motion leaves no transition after an interaction", async ({
    page,
    coordinator,
}) => {
    await page.emulateMedia({ reducedMotion: "reduce" });
    signedIn(coordinator);
    await page.goto(`/catalogs/${CATALOG_ID}`);
    await page.getByRole("tab", { name: "Settings" }).hover();
    await page.getByRole("tab", { name: "Settings" }).click();
    await page.setViewportSize({ width: 390, height: 844 });
    expect(
        await page.evaluate(
            () =>
                document
                    .getAnimations()
                    .filter((animation) => animation instanceof CSSTransition)
                    .length,
        ),
    ).toBe(0);
});

test("creates through the New menu by keyboard", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    await page.goto(`/catalogs/${CATALOG_ID}`);
    const trigger = page.getByRole("button", { name: "New" });
    await trigger.focus();
    await page.keyboard.press("Enter");
    const menu = page.getByRole("menu");
    await expect(menu).toBeVisible();
    await expect(trigger).toHaveAttribute("aria-expanded", "true");
    await page.keyboard.press("ArrowDown");
    await expect(menu.getByRole("menuitem", { name: "Catalog" })).toBeFocused();
    await expectAccessible(page);
    await page.keyboard.press("Escape");
    await expect(menu).toHaveCount(0);
    await expect(trigger).toBeFocused();

    await trigger.click();
    await page.getByRole("menuitem", { name: "Catalog" }).click();
    await expect(page).toHaveURL(/\/catalogs\/new$/);
    await expect(page.getByLabel("Connection name")).toBeVisible();
});

test("switches the theme with the segmented control", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    await page.goto(`/catalogs/${CATALOG_ID}`);
    await page
        .getByRole("region", { name: "Account" })
        .getByRole("button", { name: "Alice" })
        .click();
    const panel = page.getByRole("dialog", { name: "Alice" });
    const theme = panel.getByRole("group", { name: "Theme" });
    await theme.getByRole("button", { name: "Dark theme" }).click();
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
    await expect(
        theme.getByRole("button", { name: "Dark theme" }),
    ).toHaveAttribute("aria-pressed", "true");
    await theme.getByRole("button", { name: "Dark theme" }).click();
    await expect(
        theme.getByRole("button", { name: "Dark theme" }),
    ).toHaveAttribute("aria-pressed", "true");
});

test("collapses the rail by shortcut and remembers it", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    await page.goto(`/catalogs/${CATALOG_ID}`);
    await expect(page.getByRole("heading", { name: "lake" })).toBeVisible();
    const rail = page.getByRole("navigation", { name: "Main" });
    const expanded = await rail.boundingBox();

    await page.keyboard.press("ControlOrMeta+Backslash");
    await expect(page.locator("html")).toHaveAttribute(
        "data-sidebar",
        "collapsed",
    );
    await expect(
        page.getByRole("button", { name: "Expand sidebar" }),
    ).toBeVisible();
    // Labels leave the layout but keep naming the links.
    const catalogs = rail.getByRole("link", { name: "Catalogs" });
    await expect(catalogs).toHaveAttribute("title", "Catalogs");
    await expect
        .poll(async () => (await rail.boundingBox())?.width)
        .toBeLessThan((expanded?.width ?? 0) / 2);
    await expectAccessible(page);

    await page.reload();
    await expect(page.locator("html")).toHaveAttribute(
        "data-sidebar",
        "collapsed",
    );
    await catalogs.click();
    await expect(page).toHaveURL(/\/catalogs$/);

    await page.getByRole("button", { name: "Expand sidebar" }).click();
    await expect(page.locator("html")).not.toHaveAttribute("data-sidebar");
    await expect(catalogs).not.toHaveAttribute("title");
});

test("opens the account panel and returns focus", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    await page.goto(`/catalogs/${CATALOG_ID}`);
    const account = page
        .getByRole("region", { name: "Account" })
        .getByRole("button", { name: "Alice" });

    await account.focus();
    await page.keyboard.press("Enter");
    const panel = page.getByRole("dialog", { name: "Alice" });
    await expect(panel.getByRole("button", { name: "Sign out" })).toBeVisible();
    await expectAccessible(page);

    await page.keyboard.press("Escape");
    await expect(panel).toHaveCount(0);
    await expect(account).toBeFocused();
    await expect(account).toHaveAttribute("aria-expanded", "false");
});

test("finds a table in the command palette and opens a page", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator.on("GET", "/api/v1/search", () =>
        json(
            200,
            schema("SearchResults", {
                catalogs: [],
                tables: [
                    {
                        id: TABLE_ID,
                        catalog_id: CATALOG_ID,
                        namespace: ["sales"],
                        name: "orders",
                    },
                ],
                principals: [],
            }),
        ),
    );
    await page.goto(`/catalogs/${CATALOG_ID}`);
    await expect(page.getByRole("heading", { name: "lake" })).toBeVisible();

    await page.keyboard.press("Control+k");
    const field = page.getByRole("combobox", {
        name: "Search pages, catalogs, tables, and people",
    });
    await expect(field).toBeFocused();
    await field.fill("tables");
    const results = page.getByRole("listbox", { name: "Results" });
    await expect(
        results.getByRole("option", { name: /sales\.orders/ }),
    ).toHaveAttribute("href", `/tables/${TABLE_ID}`);
    await expect(
        results.getByRole("option", { name: "Tables" }),
    ).toHaveAttribute("aria-selected", "true");
    await expectAccessible(page);

    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(/\/tables$/);
    await expect(field).toHaveCount(0);
    expect(
        coordinator.requests.filter((request) => request.method !== "GET"),
    ).toEqual([]);
});

test("refreshes the jobs list from the live stream", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator
        .on(
            "GET",
            "/api/v1/jobs",
            () =>
                json(
                    200,
                    schema("JobPage", {
                        items: [job("Running")],
                        next_cursor: null,
                    }),
                ),
            () =>
                json(
                    200,
                    schema("JobPage", {
                        items: [job("Failed", { revision: 5 })],
                        next_cursor: null,
                    }),
                ),
        )
        // The first stream only starts; the browser reconnects after it ends,
        // once the list has loaded, and the event arrives then.
        .on(
            "GET",
            "/api/v1/events",
            () => liveStream([{ event: "ready", id: "10", data: {} }], 1_000),
            () =>
                liveStream([
                    { event: "ready", id: "10", data: {} },
                    {
                        event: "job",
                        id: "11",
                        data: {
                            job_id: JOB_ID,
                            table_id: TABLE_ID,
                            state: "Failed",
                            revision: 5,
                        },
                    },
                ]),
        );
    await page.goto("/jobs");

    await expect(
        page.getByRole("cell", { name: "Failed", exact: true }).first(),
    ).toBeVisible();
    // The browser's own reconnect resumes with `Last-Event-ID`, which
    // request interception does not expose.
    expect(coordinator.sent("GET", "/api/v1/events")).toHaveLength(2);
});
