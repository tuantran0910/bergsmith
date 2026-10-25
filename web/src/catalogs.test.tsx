import { QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "./App";
import { canManageCatalog } from "./api/auth";
import { validCatalogUri } from "./pages/catalogs/ConnectionFields";
import { createQueryClient } from "./queryClient";
import { apiError, FakeApi, json, schema } from "./test/fakes";
import {
    CATALOG_ID,
    TABLE_ID,
    catalogs,
    catalogCapabilities,
    managedCatalog,
    session,
    table,
} from "./test/fixtures";

let fake: FakeApi;
beforeEach(() => {
    fake = new FakeApi();
    vi.stubGlobal("fetch", fake.fetch);
    document.cookie = "bergsmith_csrf=csrf-value";
    fake.on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs))
        .on("GET", "/api/v1/catalogs/{id}", () => json(200, managedCatalog))
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        );
});
afterEach(() => {
    expect(fake.unrouted).toEqual([]);
});

function start(path = `/catalogs/${CATALOG_ID}`) {
    window.history.replaceState(null, "", path);
    const client = createQueryClient(0);
    render(
        <QueryClientProvider client={client}>
            <App />
        </QueryClientProvider>,
    );
    return userEvent.setup();
}

async function enterDraft(user: ReturnType<typeof userEvent.setup>) {
    await user.type(
        await screen.findByLabelText("Connection name"),
        "new-lake",
    );
    await user.type(
        screen.getByLabelText("Catalog URI"),
        "https://catalog.example",
    );
}

it("paginates connections and links to the detail", async () => {
    fake.on("GET", "/api/v1/catalogs", (request) =>
        json(
            200,
            schema("CatalogPage", {
                items: new URL(request.url).searchParams.has("cursor")
                    ? [
                          {
                              ...managedCatalog,
                              id: TABLE_ID,
                              name: "other-lake",
                              enabled: false,
                              settings: {
                                  ...managedCatalog.settings,
                                  maintenance_enabled: false,
                              },
                          },
                      ]
                    : catalogs.items,
                next_cursor: new URL(request.url).searchParams.has("cursor")
                    ? null
                    : "next-page",
            }),
        ),
    );
    const user = start("/catalogs");
    await user.click(
        await screen.findByRole("button", { name: "Load more catalogs" }),
    );
    expect(
        await screen.findByRole("link", { name: "other-lake" }),
    ).toHaveAttribute("href", `/catalogs/${TABLE_ID}`);
    await user.click(screen.getByRole("link", { name: "lake" }));
    expect(
        await screen.findByRole("heading", {
            name: "Effective storage profile",
        }),
    ).toBeVisible();
    expect(screen.getByText(/9007199254740993 s/)).toBeVisible();
    expect(screen.getByText(/exceeds this connection/)).toBeVisible();
    // The connection ID is technical metadata, not part of the name-led header.
    const technical = screen.getByText("Technical details").closest("details");
    expect(technical).not.toBeNull();
    if (technical !== null) {
        expect(within(technical).getByText(CATALOG_ID)).toBeInTheDocument();
    }
});

it("checks a draft before saving and invalidates the diagnostic after edits", async () => {
    fake.on("POST", "/api/v1/catalogs/check", () =>
        json(
            200,
            schema("DraftCatalogCapabilityCheck", {
                checked_at: "2026-10-24T00:00:00Z",
                capabilities: catalogCapabilities,
            }),
        ),
    ).on("POST", "/api/v1/catalogs", () => json(201, managedCatalog));
    const user = start("/catalogs/new");
    await enterDraft(user);
    await user.type(screen.getByLabelText("Warehouse (optional)"), "warehouse");
    await user.click(
        screen.getByRole("button", { name: "Check capabilities" }),
    );
    expect(
        await screen.findByRole("button", { name: "Save connection" }),
    ).toBeEnabled();
    expect(fake.sent("POST", "/api/v1/catalogs")).toHaveLength(0);
    await user.type(screen.getByLabelText("Connection name"), "-updated");
    expect(
        screen.queryByRole("button", { name: "Save connection" }),
    ).not.toBeInTheDocument();
    await user.click(
        screen.getByRole("button", { name: "Check capabilities" }),
    );
    await user.click(
        await screen.findByRole("button", { name: "Save connection" }),
    );
    expect(
        await screen.findByRole("heading", { name: "Connection settings" }),
    ).toBeVisible();
    const request = fake.sent("POST", "/api/v1/catalogs")[0];
    expect(request?.headers.get("x-csrf-token")).toBe("csrf-value");
    expect(await request?.json()).toEqual({
        name: "new-lake-updated",
        uri: "https://catalog.example",
        warehouse: "warehouse",
        credential_ref: null,
    });
});

it("requires acknowledgement for a diagnostic with missing required endpoints", async () => {
    fake.on("POST", "/api/v1/catalogs/check", () =>
        json(
            200,
            schema("DraftCatalogCapabilityCheck", {
                checked_at: "2026-10-24T00:00:00Z",
                capabilities: {
                    ...catalogCapabilities,
                    commit_table: "missing",
                },
            }),
        ),
    );
    const user = start("/catalogs/new");
    await enterDraft(user);
    await user.click(
        screen.getByRole("button", { name: "Check capabilities" }),
    );
    expect(
        await screen.findByRole("button", { name: "Save connection" }),
    ).toBeDisabled();
    await user.click(
        screen.getByRole("checkbox", { name: /I understand required/ }),
    );
    expect(
        screen.getByRole("button", { name: "Save connection" }),
    ).toBeEnabled();
});

it("does not replay an uncertain create", async () => {
    fake.on("POST", "/api/v1/catalogs/check", () =>
        json(
            200,
            schema("DraftCatalogCapabilityCheck", {
                checked_at: "2026-10-24T00:00:00Z",
                capabilities: catalogCapabilities,
            }),
        ),
    ).on("POST", "/api/v1/catalogs", () => json(502, {}));
    const user = start("/catalogs/new");
    await enterDraft(user);
    await user.click(
        screen.getByRole("button", { name: "Check capabilities" }),
    );
    await user.click(
        await screen.findByRole("button", { name: "Save connection" }),
    );
    expect(
        await screen.findByText(/The save may have completed/),
    ).toBeVisible();
    expect(
        screen.getByRole("button", { name: "Save connection" }),
    ).toBeDisabled();
    expect(fake.sent("POST", "/api/v1/catalogs")).toHaveLength(1);
});

it("displays failed diagnostics without creating a connection", async () => {
    fake.on("POST", "/api/v1/catalogs/check", () =>
        apiError(
            500,
            "catalog_credential_unavailable",
            "PermanentFailure",
            "configure_catalog_credential",
        ),
    );
    const user = start("/catalogs/new");
    await enterDraft(user);
    await user.type(
        screen.getByLabelText("Credential reference (optional)"),
        "vault/catalog",
    );
    await user.click(
        screen.getByRole("button", { name: "Check capabilities" }),
    );
    expect(
        await screen.findByText("Catalog credential unavailable"),
    ).toBeVisible();
    expect(
        screen.queryByRole("button", { name: "Save connection" }),
    ).not.toBeInTheDocument();
    expect(fake.sent("POST", "/api/v1/catalogs")).toHaveLength(0);
});

it("patches only changed settings, refreshes the revision, and checks capabilities", async () => {
    const updated = {
        ...managedCatalog,
        revision: 1,
        enabled: false,
        settings: {
            maintenance_enabled: false,
            max_execution_seconds: 90,
            max_unresolved_publication_seconds: 7200,
        },
    };
    fake.on("PATCH", "/api/v1/catalogs/{id}", () => json(200, updated))
        .on(
            "GET",
            "/api/v1/catalogs/{id}",
            () => json(200, managedCatalog),
            () => json(200, updated),
        )
        .on("POST", "/api/v1/catalogs/{id}/check", () =>
            json(
                200,
                schema("CatalogCapabilityCheck", {
                    catalog_id: CATALOG_ID,
                    checked_at: "2026-10-24T00:00:00Z",
                    capabilities: catalogCapabilities,
                }),
            ),
        );
    const user = start();
    expect(
        await screen.findByRole("button", { name: "Save settings" }),
    ).toBeDisabled();
    await user.click(screen.getByLabelText("Connection enabled"));
    await user.click(screen.getByLabelText("Maintenance enabled"));
    const execution = screen.getByLabelText(
        "Maximum execution duration (seconds)",
    );
    await user.clear(execution);
    await user.type(execution, "90");
    const unresolved = screen.getByLabelText(
        "Maximum unresolved publication age (seconds)",
    );
    await user.clear(unresolved);
    await user.type(unresolved, "7200");
    await user.click(screen.getByRole("button", { name: "Save settings" }));
    await waitFor(() =>
        expect(
            screen.getByRole("button", { name: "Save settings" }),
        ).toBeDisabled(),
    );
    expect(
        await fake.sent("PATCH", `/api/v1/catalogs/${CATALOG_ID}`)[0]?.json(),
    ).toEqual({
        expected_revision: 0,
        enabled: false,
        maintenance_enabled: false,
        max_execution_seconds: 90,
        max_unresolved_publication_seconds: 7200,
    });
    await user.click(
        screen.getByRole("button", { name: "Check capabilities" }),
    );
    await waitFor(() => {
        expect(
            fake.sent("POST", `/api/v1/catalogs/${CATALOG_ID}/check`),
        ).toHaveLength(1);
    });
});

it("preserves a conflicted edit until the operator explicitly reloads", async () => {
    fake.on("PATCH", "/api/v1/catalogs/{id}", () =>
        apiError(
            409,
            "revision_conflict",
            "PermanentFailure",
            "refresh_resource",
        ),
    );
    const user = start();
    await user.clear(await screen.findByLabelText("Connection name"));
    await user.type(screen.getByLabelText("Connection name"), "renamed");
    await user.click(screen.getByRole("button", { name: "Save settings" }));
    await screen.findByRole("button", { name: "Reload saved settings" });
    expect(screen.getByLabelText("Connection name")).toHaveValue("renamed");
    expect(screen.getByLabelText("Connection name")).toBeDisabled();
    await user.click(
        screen.getByRole("button", { name: "Reload saved settings" }),
    );
    await waitFor(() =>
        expect(screen.getByLabelText("Connection name")).toHaveValue("lake"),
    );
});

it("edits routing without resending unchanged settings", async () => {
    fake.on("PATCH", "/api/v1/catalogs/{id}", () =>
        json(200, { ...managedCatalog, revision: 1 }),
    );
    const user = start();
    await user.click(
        await screen.findByRole("button", { name: "Edit routing" }),
    );
    await user.clear(screen.getByLabelText("Catalog URI"));
    await user.type(
        screen.getByLabelText("Catalog URI"),
        "https://other.example",
    );
    await user.clear(screen.getByLabelText("Warehouse (optional)"));
    await user.type(
        screen.getByLabelText("Credential reference (optional)"),
        "vault/catalog",
    );
    await user.click(screen.getByRole("button", { name: "Save routing" }));
    await waitFor(() => {
        expect(
            fake.sent("PATCH", `/api/v1/catalogs/${CATALOG_ID}`),
        ).toHaveLength(1);
    });
    expect(
        await fake.sent("PATCH", `/api/v1/catalogs/${CATALOG_ID}`)[0]?.json(),
    ).toEqual({
        expected_revision: 0,
        uri: "https://other.example",
        warehouse: null,
        credential_ref: "vault/catalog",
    });
});

function discovery() {
    fake.on("GET", "/api/v1/catalogs/{id}/namespaces", (request) => {
        const query = new URL(request.url).searchParams;
        return json(
            200,
            schema("CatalogNamespacePage", {
                items: query.has("parent")
                    ? []
                    : [{ name: "sales.eu", levels: ["sales.eu"] }],
                next_cursor: null,
                skipped: 1,
            }),
        );
    })
        .on("GET", "/api/v1/catalogs/{id}/tables", () =>
            json(
                200,
                schema("CatalogTablePage", {
                    items: [
                        {
                            namespace: ["sales.eu"],
                            name: "orders",
                            registered: false,
                            table_id: null,
                            revision: null,
                        },
                    ],
                    next_cursor: null,
                    skipped: 0,
                }),
            ),
        )
        .on("POST", "/api/v1/catalogs/{id}/registrations", () =>
            json(
                200,
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
            ),
        );
}

it("preserves namespace levels and registers a table or whole namespace", async () => {
    discovery();
    const user = start();
    await user.click(
        await screen.findByRole("button", { name: "Discover tables" }),
    );
    await user.click(await screen.findByRole("button", { name: "sales.eu" }));
    await user.click(
        await screen.findByRole("button", { name: "Register orders" }),
    );
    expect(
        await screen.findByText(/Registration complete: 1 tables/),
    ).toBeVisible();
    expect(
        await fake
            .sent("POST", `/api/v1/catalogs/${CATALOG_ID}/registrations`)[0]
            ?.json(),
    ).toEqual({ namespace: ["sales.eu"], table: "orders" });
    const query = new URL(
        fake.sent("GET", `/api/v1/catalogs/${CATALOG_ID}/tables`)[0]?.url ??
            "https://invalid",
    ).searchParams;
    expect(query.getAll("namespace")).toEqual(["sales.eu"]);
    await user.click(
        screen.getByRole("button", { name: "Register entire namespace" }),
    );
    await waitFor(() => {
        expect(
            fake.sent("POST", `/api/v1/catalogs/${CATALOG_ID}/registrations`),
        ).toHaveLength(2);
    });
    expect(
        await fake
            .sent("POST", `/api/v1/catalogs/${CATALOG_ID}/registrations`)[1]
            ?.json(),
    ).toEqual({ namespace: ["sales.eu"] });
});

it("removes a ledger registration with its observed revision", async () => {
    fake.on("DELETE", "/api/v1/catalogs/{id}/registrations", () =>
        json(200, schema("CatalogUnregistration", { table_id: TABLE_ID })),
    );
    const user = start();
    await user.click(
        await screen.findByRole("button", {
            name: "Remove registration sales.orders",
        }),
    );
    const dialog = within(screen.getByRole("dialog"));
    await user.click(dialog.getByRole("button", { name: "Confirm removal" }));
    await waitFor(() =>
        expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    const request = fake.sent(
        "DELETE",
        `/api/v1/catalogs/${CATALOG_ID}/registrations`,
    )[0];
    expect(
        new URL(request?.url ?? "https://invalid").searchParams.get("revision"),
    ).toBe("7");
    expect(request?.headers.get("x-csrf-token")).toBe("csrf-value");
});

it("registers a known multi-level name when namespace browsing is unavailable", async () => {
    discovery();
    fake.on("GET", "/api/v1/catalogs/{id}/namespaces", () =>
        apiError(
            500,
            "catalog_request_rejected",
            "PermanentFailure",
            "check_catalog_compatibility",
        ),
    );
    const user = start();
    await user.click(
        await screen.findByRole("button", { name: "Discover tables" }),
    );
    expect(await screen.findByText("Catalog request rejected")).toBeVisible();
    await user.click(screen.getByText("Register by name", { exact: true }));
    await user.type(screen.getByLabelText("Namespace level 1"), "sales.eu");
    await user.click(
        screen.getByRole("button", { name: "Add namespace level" }),
    );
    await user.type(screen.getByLabelText("Namespace level 2"), "reports");
    await user.click(
        screen.getByRole("button", { name: "Add namespace level" }),
    );
    await user.click(screen.getByRole("button", { name: "Remove last level" }));
    await user.type(screen.getByLabelText("Table name"), "orders");
    await user.click(
        screen.getByRole("button", { name: "Register named table" }),
    );
    expect(await screen.findByText("Named table registered.")).toBeVisible();
    expect(
        await fake
            .sent("POST", `/api/v1/catalogs/${CATALOG_ID}/registrations`)[0]
            ?.json(),
    ).toEqual({ namespace: ["sales.eu", "reports"], table: "orders" });
});

it("follows namespace and table cursors without dropping namespace levels", async () => {
    discovery();
    fake.on("GET", "/api/v1/catalogs/{id}/namespaces", (request) => {
        const query = new URL(request.url).searchParams;
        const child = query.has("parent");
        return json(
            200,
            schema("CatalogNamespacePage", {
                items: child
                    ? []
                    : query.has("cursor")
                      ? [
                            {
                                name: "sales.eu.reports",
                                levels: ["sales.eu", "reports"],
                            },
                        ]
                      : [],
                next_cursor:
                    child || query.has("cursor") ? null : "namespace-page-2",
                skipped: 0,
            }),
        );
    }).on("GET", "/api/v1/catalogs/{id}/tables", (request) => {
        const query = new URL(request.url).searchParams;
        return json(
            200,
            schema("CatalogTablePage", {
                items: query.has("cursor")
                    ? [
                          {
                              namespace: ["sales.eu", "reports"],
                              name: "later-table",
                              registered: true,
                              table_id: TABLE_ID,
                              revision: 8,
                          },
                      ]
                    : [],
                next_cursor: query.has("cursor") ? null : "tables-page-2",
                skipped: 0,
            }),
        );
    });
    const user = start();
    await user.click(
        await screen.findByRole("button", { name: "Discover tables" }),
    );
    await user.click(
        await screen.findByRole("button", { name: "Load more namespaces" }),
    );
    await user.click(
        await screen.findByRole("button", { name: "sales.eu.reports" }),
    );
    await user.click(
        await screen.findByRole("button", { name: "Load more catalog tables" }),
    );
    expect(await screen.findByText("later-table")).toBeVisible();
    const queries = fake
        .sent("GET", `/api/v1/catalogs/${CATALOG_ID}/tables`)
        .map((request) => new URL(request.url).searchParams);
    expect(queries.at(-1)?.getAll("namespace")).toEqual([
        "sales.eu",
        "reports",
    ]);
    expect(queries.at(-1)?.get("cursor")).toBe("tables-page-2");
});

it("shows read-only inventory to viewers and denies the create route", async () => {
    fake.on("GET", "/auth/session", () => json(200, session("Viewer")));
    const user = start();
    expect(
        await screen.findByRole("heading", { name: "Registered tables" }),
    ).toBeVisible();
    expect(
        screen.queryByRole("button", { name: "Check capabilities" }),
    ).not.toBeInTheDocument();
    expect(
        screen.queryByRole("button", { name: "Discover tables" }),
    ).not.toBeInTheDocument();
    await user.click(screen.getByRole("link", { name: "All catalogs" }));
    expect(
        screen.queryByRole("link", { name: "Add Connection" }),
    ).not.toBeInTheDocument();
});

it("requires an organization grant on the direct create route", async () => {
    fake.on("GET", "/auth/session", () =>
        json(
            200,
            session("Administrator", {
                catalog_connection: CATALOG_ID,
                table: null,
            }),
        ),
    );
    start("/catalogs/new");
    expect(
        await screen.findByText(/Adding a connection requires/),
    ).toBeVisible();
});

it("checks connection scope and rejects secret-bearing or invalid URIs", () => {
    expect(canManageCatalog(session("Administrator"))).toBe(true);
    expect(canManageCatalog(session("Operator"), CATALOG_ID)).toBe(false);
    const scoped = session("Administrator", {
        catalog_connection: CATALOG_ID,
        table: null,
    });
    expect(canManageCatalog(scoped)).toBe(false);
    expect(canManageCatalog(scoped, CATALOG_ID)).toBe(true);
    expect(canManageCatalog(scoped, TABLE_ID)).toBe(false);
    for (const uri of [
        "x",
        "file:///tmp",
        "https://u:p@host",
        "https://host?",
        "https://host#",
        "https://host/a b",
    ])
        expect(validCatalogUri(uri)).toBe(false);
    expect(validCatalogUri("https://catalog.example/base")).toBe(true);
});
