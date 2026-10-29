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
import {
    type FakeCoordinator,
    expect,
    expectAccessible,
    json,
    test,
} from "./coordinator";

/** The routes the catalog detail and its access tab read. */
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
        )
        .on("GET", "/api/v1/grants", () =>
            json(
                200,
                schema("RoleGrantPage", { items: [], next_cursor: null }),
            ),
        );
}

test("reaches the main content, tabs, and a dialog by keyboard", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    await page.goto(`/catalogs/${CATALOG_ID}`);
    await expect(page.getByRole("heading", { name: "lake" })).toBeVisible();
    await expectAccessible(page);

    await page.keyboard.press("Tab");
    await expect(
        page.getByRole("link", { name: "Skip to main content" }),
    ).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(page.locator("main")).toBeFocused();

    await page.getByRole("tab", { name: "Overview" }).focus();
    await page.keyboard.press("ArrowRight");
    await expect(page.getByRole("tab", { name: "Tables" })).toBeFocused();
    await page.keyboard.press("ArrowRight");
    await expect(page.getByRole("tab", { name: "Access" })).toBeFocused();
    await page.keyboard.press("Enter");

    const grant = page.getByRole("button", { name: "Grant role" });
    await expect(grant).toBeVisible();
    await grant.focus();
    await page.keyboard.press("Enter");
    const dialog = page.getByRole("dialog");
    await expect(dialog).toBeVisible();
    await expectAccessible(page);
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
    await expect(grant).toBeFocused();
});
