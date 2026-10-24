import { schema } from "../src/test/fakes";
import {
    CATALOG_ID,
    TABLE_ID,
    catalogs,
    managedCatalog,
    session,
    table,
} from "../src/test/fixtures";
import {
    USER_ID,
    SERVICE_ID,
    TOKEN_ID,
    userPrincipal,
    servicePrincipal,
    roleGrant,
    poolToken,
    issuedPoolToken,
} from "../src/test/accessFixtures";
import { expect, expectAccessible, json, test } from "./coordinator";
import type { Page } from "@playwright/test";

async function expectDialogFits(page: Page) {
    await expectAccessible(page);
    const bounds = await page.getByRole("dialog").evaluate((dialog) => {
        const rect = dialog.getBoundingClientRect();
        return {
            top: rect.top,
            bottom: rect.bottom,
            left: rect.left,
            right: rect.right,
            height: innerHeight,
            width: innerWidth,
            scrollWidth: dialog.scrollWidth,
            clientWidth: dialog.clientWidth,
        };
    });
    expect(bounds.top).toBeGreaterThanOrEqual(15);
    expect(bounds.bottom).toBeLessThanOrEqual(bounds.height - 15);
    expect(bounds.left).toBeGreaterThanOrEqual(15);
    expect(bounds.right).toBeLessThanOrEqual(bounds.width - 15);
    expect(bounds.scrollWidth).toBeLessThanOrEqual(bounds.clientWidth);
}

test("provisions a user and grants scoped access", async ({
    page,
    coordinator,
}) => {
    let provisioned = false;
    let granted = false;
    let active = true;
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        )
        .on("GET", "/api/v1/principals", () =>
            json(
                200,
                schema("PrincipalPage", {
                    items: provisioned ? [userPrincipal] : [],
                    next_cursor: null,
                }),
            ),
        )
        .on("POST", "/api/v1/principals", () => {
            provisioned = true;
            return json(201, userPrincipal);
        })
        .on("GET", "/api/v1/principals/{id}", () =>
            json(200, { ...userPrincipal, active }),
        )
        .on("PATCH", "/api/v1/principals/{id}", () => {
            active = false;
            return json(200, { ...userPrincipal, active });
        })
        .on("GET", "/api/v1/grants", () =>
            json(
                200,
                schema("RoleGrantPage", {
                    items: granted
                        ? [
                              {
                                  ...roleGrant,
                                  role: "Operator",
                                  scope: { table: TABLE_ID },
                              },
                          ]
                        : [],
                    next_cursor: null,
                }),
            ),
        )
        .on("POST", "/api/v1/grants", () => {
            granted = true;
            return json(201, {
                ...roleGrant,
                role: "Operator",
                scope: { table: TABLE_ID },
            });
        });
    await page.emulateMedia({ reducedMotion: "reduce" });
    await page.goto("/users");
    await expect(page.getByText("No provisioned users.")).toBeVisible();
    await expectAccessible(page);
    await page
        .getByRole("button", { name: "Provision user", exact: true })
        .click();
    await page.getByLabel("Display name").fill("Bob");
    await page.getByLabel("Issuer", { exact: true }).fill(userPrincipal.issuer);
    await page
        .getByLabel("Subject", { exact: true })
        .fill(userPrincipal.subject);
    await expectAccessible(page);
    await page.getByRole("button", { name: "Provision", exact: true }).click();
    await expect(page).toHaveURL(`/users/${USER_ID}`);
    await expect(
        page.getByRole("heading", { name: "Bob", exact: true }),
    ).toBeVisible();
    await page.getByRole("button", { name: "Grant role", exact: true }).click();
    await page.getByRole("combobox", { name: "Role", exact: true }).click();
    await page.getByRole("option", { name: "Operator", exact: true }).click();
    await page.getByRole("combobox", { name: "Scope", exact: true }).click();
    await page.getByRole("option", { name: "Table", exact: true }).click();
    await page.getByLabel("Table ID", { exact: true }).fill(TABLE_ID);
    await expectAccessible(page);
    await page.getByRole("button", { name: "Save grant" }).click();
    await expect(
        page.getByRole("cell", { name: "Operator", exact: true }),
    ).toBeVisible();
    await expectAccessible(page);
    const grant = coordinator.sent("POST", "/api/v1/grants")[0];
    expect(grant?.body).toEqual({
        principal_id: USER_ID,
        role: "Operator",
        scope: { table: TABLE_ID },
    });
    expect(grant?.headers["x-csrf-token"]).toBe("csrf-value");
    await page
        .getByRole("button", { name: "Deactivate principal", exact: true })
        .click();
    await expectAccessible(page);
    await page
        .getByRole("button", { name: "Confirm deactivate principal" })
        .click();
    await expect(page.getByText("Deactivated", { exact: true })).toBeVisible();
    await page.setViewportSize({ width: 390, height: 844 });
    await expectAccessible(page);
    expect(
        await page.evaluate(
            () => document.documentElement.scrollWidth <= window.innerWidth,
        ),
    ).toBe(true);
});

test("issues and revokes a pool token", async ({ page, coordinator }) => {
    let issued = false;
    let revoked = false;
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/service-identities/{id}", () =>
            json(200, servicePrincipal),
        )
        .on("GET", "/api/v1/grants", () =>
            json(
                200,
                schema("RoleGrantPage", { items: [], next_cursor: null }),
            ),
        )
        .on("GET", "/api/v1/service-identities/{id}/tokens", () =>
            json(
                200,
                schema("WorkerPoolTokenPage", {
                    items: issued ? [{ ...poolToken, active: !revoked }] : [],
                    next_cursor: null,
                }),
            ),
        )
        .on("POST", "/api/v1/service-identities/{id}/tokens", () => {
            issued = true;
            return json(201, issuedPoolToken);
        })
        .on("DELETE", "/api/v1/service-identities/{id}/tokens/{token}", () => {
            revoked = true;
            return json(200, { ...poolToken, active: false });
        });
    await page.emulateMedia({ reducedMotion: "reduce" });
    await page.goto(`/service-identities/${SERVICE_ID}`);
    await page.getByLabel("Worker pool", { exact: true }).fill("default");
    await page
        .getByLabel("Token label (optional)", { exact: true })
        .fill("Worker fleet");
    await page.getByRole("button", { name: "Issue token" }).click();
    await expect(page.getByLabel("Pool bearer token")).toHaveText(
        issuedPoolToken.token,
    );
    await expectAccessible(page);
    await page.getByRole("button", { name: "I have saved the token" }).click();
    await expect(page.getByLabel("Pool bearer token")).toHaveCount(0);
    await expectAccessible(page);
    await page
        .getByRole("button", { name: "Revoke token", exact: true })
        .click();
    await expect(page.getByRole("dialog")).toContainText(TOKEN_ID);
    await expectAccessible(page);
    await page.getByRole("button", { name: "Confirm revoke token" }).click();
    await expect(page.getByText("Revoked", { exact: true })).toBeVisible();
    await page.reload();
    await expect(page.getByText("Revoked", { exact: true })).toBeVisible();
    await expect(
        page.getByText(issuedPoolToken.token, { exact: true }),
    ).toHaveCount(0);
});

test("connection and table Access keep grants scoped on narrow screens", async ({
    page,
    coordinator,
}) => {
    coordinator
        .on("GET", "/auth/session", () =>
            json(
                200,
                session("Administrator", {
                    catalog_connection: CATALOG_ID,
                    table: null,
                }),
            ),
        )
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs))
        .on("GET", "/api/v1/catalogs/{id}", () => json(200, managedCatalog))
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        )
        .on("GET", "/api/v1/tables/{id}", () => json(200, table))
        .on("GET", "/api/v1/grants", () =>
            json(
                200,
                schema("RoleGrantPage", { items: [], next_cursor: null }),
            ),
        );
    await page.setViewportSize({ width: 390, height: 844 });
    await page.emulateMedia({ reducedMotion: "reduce" });
    await page.goto(`/catalogs/${CATALOG_ID}`);
    await page.getByRole("button", { name: "Access", exact: true }).click();
    await expect(
        page.getByRole("link", { name: "Users", exact: true }),
    ).toHaveCount(0);
    await page.getByRole("button", { name: "Grant role", exact: true }).click();
    await page.getByLabel("Principal ID", { exact: true }).fill(USER_ID);
    await expect(page.getByRole("dialog")).toContainText("Connection lake");
    await expect(page.getByLabel("Scope", { exact: true })).toHaveCount(0);
    await expectAccessible(page);
    await page.getByRole("button", { name: "Close", exact: true }).click();
    await page.goto(`/tables/${TABLE_ID}`);
    await page.getByRole("button", { name: "Access", exact: true }).click();
    await expect(page.getByText(/Direct table grants/)).toBeVisible();
    await expectAccessible(page);
    expect(
        coordinator
            .sent("GET", "/api/v1/grants")
            .map((request) => new URLSearchParams(request.search).get("table")),
    ).toContain(TABLE_ID);
});

test("scrolls the scope picker and saves a grant on a short screen", async ({
    page,
    coordinator,
}) => {
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/principals/{id}", () => json(200, userPrincipal))
        .on("GET", "/api/v1/grants", () =>
            json(
                200,
                schema("RoleGrantPage", { items: [], next_cursor: null }),
            ),
        )
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        )
        .on("POST", "/api/v1/grants", () =>
            json(201, { ...roleGrant, scope: { table: TABLE_ID } }),
        );
    await page.setViewportSize({ width: 390, height: 320 });
    await page.goto(`/users/${USER_ID}`);
    await page.getByRole("button", { name: "Grant role", exact: true }).click();
    await page.getByRole("combobox", { name: "Scope", exact: true }).click();
    await page.getByRole("option", { name: "Table", exact: true }).click();
    await page.getByLabel("Table ID", { exact: true }).fill(TABLE_ID);
    await expectDialogFits(page);
    await page.getByRole("button", { name: "Save grant" }).click();
    await expect(page.getByRole("dialog")).toHaveCount(0);
    expect(coordinator.sent("POST", "/api/v1/grants")[0]?.body).toEqual({
        principal_id: USER_ID,
        role: "Viewer",
        scope: { table: TABLE_ID },
    });
});

test("wraps a long principal name in its confirmation and deactivates it", async ({
    page,
    coordinator,
}) => {
    const display_name = "N".repeat(200);
    let active = true;
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/principals/{id}", () =>
            json(200, { ...userPrincipal, display_name, active }),
        )
        .on("GET", "/api/v1/grants", () =>
            json(
                200,
                schema("RoleGrantPage", { items: [], next_cursor: null }),
            ),
        )
        .on("PATCH", "/api/v1/principals/{id}", () => {
            active = false;
            return json(200, { ...userPrincipal, display_name, active });
        });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(`/users/${USER_ID}`);
    await page
        .getByRole("button", { name: "Deactivate principal", exact: true })
        .click();
    await expectDialogFits(page);
    await expect(page.getByRole("dialog")).toContainText(display_name);
    const confirm = page.getByRole("button", {
        name: "Confirm deactivate principal",
    });
    const buttonBounds = await confirm.boundingBox();
    expect(buttonBounds?.width).toBeLessThan(358);
    await confirm.click();
    await expect(page.getByText("Deactivated", { exact: true })).toBeVisible();
});

test("wraps a long token label on narrow screens and revokes the token", async ({
    page,
    coordinator,
}) => {
    const label = "T".repeat(200);
    let active = true;
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/service-identities/{id}", () =>
            json(200, servicePrincipal),
        )
        .on("GET", "/api/v1/grants", () =>
            json(
                200,
                schema("RoleGrantPage", { items: [], next_cursor: null }),
            ),
        )
        .on("GET", "/api/v1/service-identities/{id}/tokens", () =>
            json(
                200,
                schema("WorkerPoolTokenPage", {
                    items: [{ ...poolToken, label, active }],
                    next_cursor: null,
                }),
            ),
        )
        .on("DELETE", "/api/v1/service-identities/{id}/tokens/{token}", () => {
            active = false;
            return json(200, { ...poolToken, label, active });
        });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto(`/service-identities/${SERVICE_ID}`);
    await expect(page.getByText(label, { exact: true })).toBeVisible();
    expect(
        await page.evaluate(
            () => document.documentElement.scrollWidth <= innerWidth,
        ),
    ).toBe(true);
    await page
        .getByRole("button", { name: "Revoke token", exact: true })
        .click();
    await expectDialogFits(page);
    await page.getByRole("button", { name: "Confirm revoke token" }).click();
    await expect(page.getByText("Revoked", { exact: true })).toBeVisible();
});

test("uses named grants, compact selectors, uppercase navigation, and pointer cursors", async ({
    page,
    coordinator,
}) => {
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs))
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        )
        .on("GET", "/api/v1/principals/{id}", () => json(200, userPrincipal))
        .on("GET", "/api/v1/grants", () =>
            json(
                200,
                schema("RoleGrantPage", {
                    items: [
                        {
                            ...roleGrant,
                            principal_name: "Bob Chen",
                            scope: { table: TABLE_ID },
                            scope_name: "lake.sales.orders",
                        },
                    ],
                    next_cursor: null,
                }),
            ),
        );
    await page.goto("/catalogs");
    const add = page.getByRole("link", { name: "Add Connection", exact: true });
    await expect(add).toBeVisible();
    await expect(add).toHaveCSS("cursor", "pointer");
    expect((await add.boundingBox())?.height).toBeGreaterThanOrEqual(36);
    await expect(
        page
            .getByRole("navigation", { name: "Main" })
            .getByRole("link", { name: "Users", exact: true }),
    ).toHaveCSS("text-transform", "uppercase");
    await page.goto("/tables");
    await expect(page.getByRole("columnheader").first()).toHaveText(
        "Table UUID",
    );
    const copy = page.getByRole("button", {
        name: "Copy table UUID",
        exact: true,
    });
    await copy.hover();
    await expect(copy).toHaveCSS("cursor", "pointer");
    await page.goto(`/users/${USER_ID}`);
    await expect(page.getByRole("cell", { name: /Bob Chen/ })).toBeVisible();
    await expect(
        page.getByText("Table lake.sales.orders", { exact: true }),
    ).toBeVisible();
    await expect(page.getByRole("row", { name: /Bob Chen/ })).not.toContainText(
        USER_ID,
    );
    const grant = page.getByRole("button", { name: "Grant role", exact: true });
    await expect(grant).toHaveCSS("cursor", "pointer");
    await grant.click();
    const backdrop = page.locator('[data-slot="dialog-overlay"]');
    const coverage = await backdrop.evaluate((element) => ({
        right: element.getBoundingClientRect().right,
        width: innerWidth,
    }));
    expect(coverage.right).toBe(coverage.width);
    const role = page.getByRole("combobox", { name: "Role", exact: true });
    await role.click();
    await expect(role).toHaveCSS("cursor", "pointer");
    const menu = page.getByRole("listbox");
    await expect(menu).toBeVisible();
    await expect(
        page.getByRole("option", { name: "Maintenance Service", exact: true }),
    ).toHaveCSS("cursor", "pointer");
    expect((await menu.boundingBox())?.height).toBeLessThanOrEqual(150);
    expect((await menu.boundingBox())?.width).toBeLessThanOrEqual(
        ((await role.boundingBox())?.width ?? 0) + 1,
    );
    await expectAccessible(page);
    await role.press("Escape");
    await expect(menu).toHaveCount(0);
    await expect(page.getByRole("dialog")).toBeVisible();
});
