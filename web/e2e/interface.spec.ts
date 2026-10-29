// The interface tokens (BS-033.T1, ADR 0067): the font is served by the
// deployment's own origin and preloaded, the type scale sets 13 px body text,
// and reduced motion leaves no transition behind an interaction.

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
