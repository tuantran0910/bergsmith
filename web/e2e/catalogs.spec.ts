import { catalogStorage } from "../src/test/fixtures";
import { schema } from "../src/test/fakes";
import {
    CATALOG_ID,
    TABLE_ID,
    catalogCapabilities,
    catalogTotals,
    catalogs,
    managedCatalog,
    session,
    table,
} from "../src/test/fixtures";
import { expect, expectAccessible, json, test } from "./coordinator";

test("checks an unsaved connection, saves it, and registers a discovered table", async ({
    page,
    coordinator,
}) => {
    let saved = false;
    let registered = false;
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/catalogs", () =>
            json(
                200,
                schema("CatalogPage", {
                    items: saved ? [managedCatalog] : [],
                    next_cursor: null,
                }),
            ),
        )
        .on("POST", "/api/v1/catalogs/check", () =>
            json(
                200,
                schema("DraftCatalogCapabilityCheck", {
                    checked_at: "2026-10-24T00:00:00Z",
                    capabilities: catalogCapabilities,
                }),
            ),
        )
        .on("POST", "/api/v1/catalogs", () => {
            saved = true;
            return json(201, managedCatalog);
        })
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
                schema("TablePage", {
                    items: registered ? [table] : [],
                    next_cursor: null,
                }),
            ),
        )
        .on("GET", "/api/v1/catalogs/{id}/namespaces", (request) =>
            json(
                200,
                schema("CatalogNamespacePage", {
                    items: new URLSearchParams(request.search).has("parent")
                        ? []
                        : [{ name: "sales.eu", levels: ["sales.eu"] }],
                    next_cursor: null,
                    skipped: 0,
                }),
            ),
        )
        .on("GET", "/api/v1/catalogs/{id}/tables", () =>
            json(
                200,
                schema("CatalogTablePage", {
                    items: [
                        {
                            namespace: ["sales.eu"],
                            name: "orders",
                            registered,
                            table_id: registered ? TABLE_ID : null,
                            revision: registered ? 7 : null,
                        },
                    ],
                    next_cursor: null,
                    skipped: 0,
                }),
            ),
        )
        .on("POST", "/api/v1/catalogs/{id}/registrations", () => {
            registered = true;
            return json(
                201,
                schema("CatalogRegistration", {
                    registered: [
                        {
                            namespace: ["sales.eu"],
                            name: "orders",
                            table_id: TABLE_ID,
                            created: true,
                        },
                    ],
                }),
            );
        });

    await page.goto("/catalogs");
    await expect(
        page.getByText("No catalogs are visible to you."),
    ).toBeVisible();
    await expectAccessible(page);
    await page.getByRole("link", { name: "Add Connection" }).click();
    await page.getByLabel("Connection name").fill("new-lake");
    await page.getByLabel("Catalog URI").fill("https://catalog.example");
    await expectAccessible(page);
    await page.getByRole("button", { name: "Check capabilities" }).click();
    await expect(
        page.getByRole("button", { name: "Save connection" }),
    ).toBeEnabled();
    expect(coordinator.sent("POST", "/api/v1/catalogs")).toHaveLength(0);
    await expectAccessible(page);
    await page.getByRole("button", { name: "Save connection" }).click();
    await expect(page).toHaveURL(`/catalogs/${CATALOG_ID}`);
    await expect(
        page
            .getByRole("navigation", { name: "Breadcrumb" })
            .getByText("lake", { exact: true }),
    ).toBeVisible();
    await expect(
        page.getByRole("heading", { name: "Effective storage profile" }),
    ).toBeVisible();
    await expect(page.getByLabel("Connection enabled")).toHaveCSS(
        "cursor",
        "pointer",
    );
    await expect(page.getByLabel("Maintenance enabled")).toHaveCSS(
        "cursor",
        "pointer",
    );
    await expectAccessible(page);
    await page.getByRole("button", { name: "Discover tables" }).click();
    await page.getByRole("button", { name: "sales.eu", exact: true }).click();
    await expectAccessible(page);
    await page.getByRole("button", { name: "Register orders" }).click();
    await expect(
        page.getByText("Registration complete: 1 tables (1 newly registered)."),
    ).toBeVisible();
    await expect(page.getByText("Registered", { exact: true })).toBeVisible();
    await expectAccessible(page);
    const check = coordinator.sent("POST", "/api/v1/catalogs/check")[0];
    const create = coordinator.sent("POST", "/api/v1/catalogs")[0];
    expect(check?.body).toEqual(create?.body);
    expect(check?.headers["x-csrf-token"]).toBe("csrf-value");
    const registration = coordinator.sent(
        "POST",
        `/api/v1/catalogs/${CATALOG_ID}/registrations`,
    )[0];
    expect(registration?.body).toEqual({
        namespace: ["sales.eu"],
        table: "orders",
    });
    expect(registration?.headers["x-csrf-token"]).toBe("csrf-value");
});

test("pauses maintenance and removes a registration with its revision", async ({
    page,
    coordinator,
}) => {
    let current = managedCatalog;
    let removed = false;
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs))
        .on("GET", "/api/v1/catalogs/{id}", () => json(200, current))
        .on("GET", "/api/v1/catalogs/{id}/storage", () =>
            json(200, catalogStorage),
        )
        .on("GET", "/api/v1/catalogs/{id}/statistics", () =>
            json(200, catalogTotals),
        )
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", {
                    items: removed ? [] : [table],
                    next_cursor: null,
                }),
            ),
        )
        .on("PATCH", "/api/v1/catalogs/{id}", () => {
            current = {
                ...current,
                revision: 1,
                settings: { ...current.settings, maintenance_enabled: false },
            };
            return json(200, current);
        })
        .on("DELETE", "/api/v1/catalogs/{id}/registrations", () => {
            removed = true;
            return json(
                200,
                schema("CatalogUnregistration", { table_id: TABLE_ID }),
            );
        });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(`/catalogs/${CATALOG_ID}`);
    const budget = page.getByLabel("Maximum execution duration (seconds)");
    await budget.fill("");
    await page.getByRole("button", { name: "Save settings" }).click();
    expect(
        coordinator.sent("PATCH", `/api/v1/catalogs/${CATALOG_ID}`),
    ).toHaveLength(0);
    await budget.fill("180");
    await page.getByLabel("Maintenance enabled").uncheck();
    await page.getByRole("button", { name: "Save settings" }).click();
    await expect(
        page.getByText("Maintenance paused", { exact: true }),
    ).toBeVisible();
    await expectAccessible(page);
    expect(
        coordinator.sent("PATCH", `/api/v1/catalogs/${CATALOG_ID}`)[0]?.body,
    ).toEqual({ expected_revision: 0, maintenance_enabled: false });
    await page
        .getByRole("button", {
            name: "Remove registration sales.orders",
            exact: true,
        })
        .click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toBeVisible();
    // Measure the dialog after its opening transition, not while its text is transparent.
    await dialog.evaluate(async (element) => {
        await Promise.all(
            element
                .getAnimations({ subtree: true })
                .map((animation) => animation.finished),
        );
    });
    await expectAccessible(page);
    await dialog.getByRole("button", { name: "Confirm removal" }).click();
    await expect(dialog).toHaveCount(0);
    await expect(
        page.getByText("No tables are registered on this connection."),
    ).toBeVisible();
    const request = coordinator.sent(
        "DELETE",
        `/api/v1/catalogs/${CATALOG_ID}/registrations`,
    )[0];
    const query = new URLSearchParams(request?.search);
    expect(query.get("revision")).toBe("7");
    expect(query.getAll("namespace")).toEqual(["sales"]);
    await expectAccessible(page);
});

test("blocks secret-bearing URIs before sending a draft to the coordinator", async ({
    page,
    coordinator,
}) => {
    coordinator.on("GET", "/auth/session", () =>
        json(200, session("Administrator")),
    );
    await page.goto("/catalogs/new");
    await page.getByLabel("Connection name").fill("lake");
    await page
        .getByLabel("Catalog URI")
        .fill("https://user:secret@catalog.example?token=secret");
    await page.getByRole("button", { name: "Check capabilities" }).click();
    expect(coordinator.sent("POST", "/api/v1/catalogs/check")).toHaveLength(0);
    await expect(
        page.getByRole("button", { name: "Save connection" }),
    ).toHaveCount(0);
    await expectAccessible(page);
});

test("shows catalog storage coverage and shared-storage scope", async ({
    page,
    coordinator,
}) => {
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
    await page.goto(`/catalogs/${CATALOG_ID}`);
    await expect(
        page.getByText(/1 of 2 visible tables assessed/),
    ).toBeVisible();
    await expect(
        page.getByText(/Shared files and overlapping table locations/),
    ).toBeVisible();
    await expectAccessible(page);
});
