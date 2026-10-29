import { catalogStorage } from "./test/fixtures";
import { QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App } from "./App";
import { SESSION_KEY, createQueryClient, setSession } from "./queryClient";
import { canManageScope } from "./pages/access/Grants";
import { apiError, FakeApi, json, schema } from "./test/fakes";
import {
    CATALOG_ID,
    TABLE_ID,
    catalogs,
    catalogTotals,
    managedCatalog,
    session,
    table,
} from "./test/fixtures";
import {
    USER_ID,
    SERVICE_ID,
    TOKEN_ID,
    issuedPoolToken,
    poolToken,
    roleGrant,
    servicePrincipal,
    userPrincipal,
} from "./test/accessFixtures";

let fake: FakeApi;
beforeEach(() => {
    fake = new FakeApi();
    vi.stubGlobal("fetch", fake.fetch);
    document.cookie = "bergsmith_csrf=csrf-value";
    fake.on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/principals", () =>
            json(
                200,
                schema("PrincipalPage", {
                    items: [userPrincipal],
                    next_cursor: null,
                }),
            ),
        )
        .on("GET", "/api/v1/principals/{id}", () => json(200, userPrincipal))
        .on("GET", "/api/v1/service-identities", () =>
            json(
                200,
                schema("PrincipalPage", {
                    items: [servicePrincipal],
                    next_cursor: null,
                }),
            ),
        )
        .on("GET", "/api/v1/service-identities/{id}", () =>
            json(200, servicePrincipal),
        )
        .on("GET", "/api/v1/service-identities/{id}/tokens", () =>
            json(
                200,
                schema("WorkerPoolTokenPage", { items: [], next_cursor: null }),
            ),
        )
        .on("GET", "/api/v1/grants", () =>
            json(
                200,
                schema("RoleGrantPage", { items: [], next_cursor: null }),
            ),
        )
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs))
        .on("GET", "/api/v1/catalogs/{id}", () => json(200, managedCatalog))
        .on("GET", "/api/v1/catalogs/{id}/storage", () =>
            json(200, catalogStorage),
        )
        .on("GET", "/api/v1/catalogs/{id}/statistics", () =>
            json(200, catalogTotals),
        )
        .on("GET", "/api/v1/tables/{id}", () => json(200, table));
    fake.on("GET", "/api/v1/tables", () =>
        json(200, schema("TablePage", { items: [table], next_cursor: null })),
    );
});
afterEach(() => {
    expect(fake.unrouted).toEqual([]);
});
function start(path = "/users") {
    window.history.replaceState(null, "", path);
    const client = createQueryClient(0);
    render(
        <QueryClientProvider client={client}>
            <App />
        </QueryClientProvider>,
    );
    return { user: userEvent.setup(), client };
}

it("provisions a user and grants a table role using the observed scope", async () => {
    fake.on("POST", "/api/v1/principals", () => json(201, userPrincipal)).on(
        "POST",
        "/api/v1/grants",
        () => json(201, roleGrant),
    );
    const { user } = start();
    await user.click(
        await screen.findByRole("button", { name: "Provision user" }),
    );
    await user.type(screen.getByLabelText("Display name"), "Bob");
    await user.type(screen.getByLabelText("Issuer"), userPrincipal.issuer);
    await user.type(screen.getByLabelText("Subject"), userPrincipal.subject);
    await user.click(screen.getByRole("button", { name: "Provision" }));
    await user.click(await screen.findByRole("button", { name: "Grant role" }));
    await user.click(screen.getByRole("combobox", { name: "Role" }));
    await user.click(screen.getByRole("option", { name: "Operator" }));
    await waitFor(() => {
        expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    });
    await user.click(screen.getByRole("combobox", { name: "Scope" }));
    await user.click(await screen.findByRole("option", { name: "Table" }));
    await user.type(screen.getByLabelText("Table ID"), TABLE_ID);
    await user.click(screen.getByRole("button", { name: "Save grant" }));
    await waitFor(() =>
        expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    const provision = fake.sent("POST", "/api/v1/principals")[0];
    expect(await provision?.json()).toEqual({
        issuer: userPrincipal.issuer,
        subject: userPrincipal.subject,
        display_name: "Bob",
    });
    const grant = fake.sent("POST", "/api/v1/grants")[0];
    expect(await grant?.json()).toEqual({
        principal_id: USER_ID,
        role: "Operator",
        scope: { table: TABLE_ID },
    });
    expect(grant?.headers.get("x-csrf-token")).toBe("csrf-value");
});

it("follows principal cursors and restricts the directory to users", async () => {
    fake.on("GET", "/api/v1/principals", (req) =>
        json(
            200,
            schema("PrincipalPage", {
                items: new URL(req.url).searchParams.has("cursor")
                    ? [
                          {
                              ...userPrincipal,
                              id: SERVICE_ID,
                              display_name: "Second user",
                          },
                      ]
                    : [userPrincipal],
                next_cursor: new URL(req.url).searchParams.has("cursor")
                    ? null
                    : "next-users",
            }),
        ),
    );
    const { user } = start();
    await user.click(
        await screen.findByRole("button", { name: "Load more users" }),
    );
    expect(
        await screen.findByRole("link", { name: "Second user" }),
    ).toBeVisible();
    expect(
        new URL(
            fake.sent("GET", "/api/v1/principals")[1]?.url ?? "",
        ).searchParams.get("cursor"),
    ).toBe("next-users");
    expect(
        new URL(
            fake.sent("GET", "/api/v1/principals")[0]?.url ?? "",
        ).searchParams.get("kind"),
    ).toBe("User");
});

it("requires confirmation for deactivation and preserves the last-administrator error", async () => {
    fake.on("PATCH", "/api/v1/principals/{id}", () =>
        apiError(
            409,
            "last_administrator",
            "PermanentFailure",
            "add_another_administrator",
        ),
    );
    const { user } = start(`/users/${USER_ID}`);
    const technical = (await screen.findByText("Technical details")).closest(
        "details",
    );
    expect(technical).not.toBeNull();
    if (technical !== null) {
        expect(within(technical).getByText(USER_ID)).toBeInTheDocument();
    }
    await user.click(
        await screen.findByRole("button", { name: "Deactivate principal" }),
    );
    expect(fake.sent("PATCH", `/api/v1/principals/${USER_ID}`)).toHaveLength(0);
    expect(screen.getByRole("dialog")).toHaveTextContent(
        "running jobs continue",
    );
    await user.click(
        screen.getByRole("button", { name: "Confirm deactivate principal" }),
    );
    expect(await screen.findByRole("alert")).toHaveTextContent(
        /Last administrator/i,
    );
});

it("updates active state after a successful deactivation and allows reactivation", async () => {
    let active = true;
    fake.on("GET", "/api/v1/principals/{id}", () =>
        json(200, { ...userPrincipal, active }),
    ).on("PATCH", "/api/v1/principals/{id}", async (req) => {
        active = ((await req.json()) as { active: boolean }).active;
        return json(200, { ...userPrincipal, active });
    });
    const { user } = start(`/users/${USER_ID}`);
    await user.click(
        await screen.findByRole("button", { name: "Deactivate principal" }),
    );
    await user.click(
        screen.getByRole("button", { name: "Confirm deactivate principal" }),
    );
    await user.click(
        await screen.findByRole("button", { name: "Reactivate principal" }),
    );
    await user.click(
        screen.getByRole("button", { name: "Confirm reactivate principal" }),
    );
    expect(
        await screen.findByRole("button", { name: "Deactivate principal" }),
    ).toBeVisible();
});

it("scoped administrators can manage a connection's Access without directory access", async () => {
    fake.on("GET", "/auth/session", () =>
        json(
            200,
            session("Administrator", {
                catalog_connection: CATALOG_ID,
                table: null,
            }),
        ),
    );
    fake.on("POST", "/api/v1/grants", () =>
        json(201, { ...roleGrant, scope: { catalog_connection: CATALOG_ID } }),
    );
    const { user } = start(`/catalogs/${CATALOG_ID}`);
    await user.click(await screen.findByRole("tab", { name: "Access" }));
    expect(
        screen.queryByRole("link", { name: "Users" }),
    ).not.toBeInTheDocument();
    await user.click(await screen.findByRole("button", { name: "Grant role" }));
    await user.type(screen.getByLabelText("Principal ID"), USER_ID);
    expect(screen.queryByLabelText("Scope")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Save grant" }));
    await waitFor(() => {
        expect(fake.sent("POST", "/api/v1/grants")).toHaveLength(1);
    });
    expect(await fake.sent("POST", "/api/v1/grants")[0]?.json()).toMatchObject({
        scope: { catalog_connection: CATALOG_ID },
    });
    expect(fake.sent("GET", "/api/v1/principals")).toHaveLength(0);
});

it("table Access is permission scoped and disappears on permission removal", async () => {
    const { client } = start(`/tables/${TABLE_ID}?tab=access`);
    expect(await screen.findByText(/Direct table grants/)).toBeVisible();
    expect(
        new URL(
            fake.sent("GET", "/api/v1/grants")[0]?.url ?? "",
        ).searchParams.get("table"),
    ).toBe(TABLE_ID);
    fake.on("GET", "/auth/session", () => json(200, session("Viewer")));
    act(() => {
        client.setQueryData(SESSION_KEY, session("Viewer"));
    });
    await waitFor(() =>
        expect(
            screen.queryByRole("button", { name: "Grant role" }),
        ).not.toBeInTheDocument(),
    );
});

it("opens token management from the URL and keeps it out of a user's tabs", async () => {
    const { user } = start(`/service-identities/${SERVICE_ID}?tab=tokens`);
    expect(
        await screen.findByRole("heading", { name: "Pool tokens" }),
    ).toBeVisible();
    expect(screen.getByRole("tab", { name: "Tokens" })).toHaveAttribute(
        "aria-selected",
        "true",
    );
    await user.click(screen.getByRole("tab", { name: "Access" }));
    expect(window.location.search).toBe("");
});

it("ignores a token tab for a user principal", async () => {
    start(`/users/${USER_ID}?tab=tokens`);
    expect(await screen.findByRole("tab", { name: "Access" })).toHaveAttribute(
        "aria-selected",
        "true",
    );
    expect(
        screen.queryByRole("tab", { name: "Tokens" }),
    ).not.toBeInTheDocument();
});

it("hides directory pages and avoids protected requests for viewers", async () => {
    fake.on("GET", "/auth/session", () => json(200, session("Viewer")));
    start("/users");
    expect(await screen.findByRole("alert")).toHaveTextContent("grants:manage");
    expect(fake.sent("GET", "/api/v1/principals")).toHaveLength(0);
});

it("blocks duplicate provisioning after an uncertain outcome", async () => {
    fake.on("POST", "/api/v1/principals", () => json(504, {}));
    const { user } = start();
    await user.click(
        await screen.findByRole("button", { name: "Provision user" }),
    );
    await user.type(screen.getByLabelText("Display name"), "Bob");
    await user.type(screen.getByLabelText("Issuer"), userPrincipal.issuer);
    await user.type(screen.getByLabelText("Subject"), userPrincipal.subject);
    await user.click(screen.getByRole("button", { name: "Provision" }));
    await waitFor(() =>
        expect(
            screen.getByRole("button", { name: "Provision" }),
        ).toBeDisabled(),
    );
    expect(fake.sent("POST", "/api/v1/principals")).toHaveLength(1);
    expect(screen.getByLabelText("Display name")).toHaveValue("Bob");
});

it("issues a token once without leaving its bearer in query or mutation caches", async () => {
    let issued = false;
    fake.on("POST", "/api/v1/service-identities/{id}/tokens", () => {
        issued = true;
        return json(201, issuedPoolToken);
    }).on("GET", "/api/v1/service-identities/{id}/tokens", () =>
        json(
            200,
            schema("WorkerPoolTokenPage", {
                items: issued ? [poolToken] : [],
                next_cursor: null,
            }),
        ),
    );
    const { user, client } = start(`/service-identities/${SERVICE_ID}`);
    await user.click(await screen.findByRole("tab", { name: "Tokens" }));
    await user.type(await screen.findByLabelText("Worker pool"), "default");
    await user.click(screen.getByRole("button", { name: "Issue token" }));
    expect(await screen.findByLabelText("Pool bearer token")).toHaveTextContent(
        issuedPoolToken.token,
    );
    expect(
        JSON.stringify(
            client
                .getQueryCache()
                .getAll()
                .map((query) => query.state.data),
        ),
    ).not.toContain(issuedPoolToken.token);
    expect(
        JSON.stringify(
            client
                .getMutationCache()
                .getAll()
                .map((mutation) => mutation.state.data),
        ),
    ).not.toContain(issuedPoolToken.token);
    await user.click(
        screen.getByRole("button", { name: "I have saved the token" }),
    );
    await waitFor(() =>
        expect(
            screen.queryByLabelText("Pool bearer token"),
        ).not.toBeInTheDocument(),
    );
    expect(await screen.findByText("Worker fleet")).toBeVisible();
});

it("creates a service identity and opens its token management", async () => {
    fake.on("POST", "/api/v1/service-identities", () =>
        json(201, servicePrincipal),
    );
    const { user } = start("/service-identities");
    await user.click(
        await screen.findByRole("button", { name: "Create service identity" }),
    );
    await user.type(
        screen.getByLabelText("Display name"),
        servicePrincipal.display_name,
    );
    await user.click(screen.getByRole("button", { name: "Create identity" }));
    expect(
        await screen.findByRole("heading", { name: "Pool tokens" }),
    ).toBeVisible();
});

it("rotates a token with confirmation and shows the replacement once", async () => {
    let rotated = false;
    fake.on("GET", "/api/v1/service-identities/{id}/tokens", () =>
        json(
            200,
            schema("WorkerPoolTokenPage", {
                items: [{ ...poolToken, active: !rotated }],
                next_cursor: null,
            }),
        ),
    ).on(
        "POST",
        "/api/v1/service-identities/{id}/tokens/{token}/rotate",
        () => {
            rotated = true;
            return json(201, {
                ...issuedPoolToken,
                id: USER_ID,
                rotated_from: TOKEN_ID,
            });
        },
    );
    const { user } = start(`/service-identities/${SERVICE_ID}`);
    await user.click(await screen.findByRole("tab", { name: "Tokens" }));
    await user.click(
        await screen.findByRole("button", { name: "Rotate token" }),
    );
    const rotate = screen.getByRole("dialog");
    expect(rotate).toHaveTextContent("Worker fleet (pool default)");
    expect(rotate).not.toHaveTextContent(TOKEN_ID);
    expect(rotate).toHaveTextContent("old token stops working immediately");
    await user.click(
        screen.getByRole("button", { name: "Confirm rotate token" }),
    );
    expect(await screen.findByLabelText("Pool bearer token")).toBeVisible();
    await user.click(
        screen.getByRole("button", { name: "I have saved the token" }),
    );
    expect(await screen.findByText("Revoked")).toBeVisible();
});

it("revokes a token while keeping the metadata and blocks issuance for inactive services", async () => {
    let revoked = false;
    fake.on("GET", "/api/v1/service-identities/{id}", () =>
        json(200, { ...servicePrincipal, active: false }),
    )
        .on("GET", "/api/v1/service-identities/{id}/tokens", () =>
            json(
                200,
                schema("WorkerPoolTokenPage", {
                    items: [{ ...poolToken, active: !revoked }],
                    next_cursor: null,
                }),
            ),
        )
        .on("DELETE", "/api/v1/service-identities/{id}/tokens/{token}", () => {
            revoked = true;
            return json(200, { ...poolToken, active: false });
        });
    const { user } = start(`/service-identities/${SERVICE_ID}`);
    await user.click(await screen.findByRole("tab", { name: "Tokens" }));
    await user.click(
        await screen.findByRole("button", { name: "Revoke token" }),
    );
    expect(
        screen.queryByRole("button", { name: "Issue token" }),
    ).not.toBeInTheDocument();
    expect(
        screen.queryByRole("button", { name: "Rotate token" }),
    ).not.toBeInTheDocument();
    await user.click(
        screen.getByRole("button", { name: "Confirm revoke token" }),
    );
    expect(await screen.findByText("Revoked")).toBeVisible();
});

it("revokes a grant and refreshes the active list", async () => {
    let revoked = false;
    fake.on("GET", "/api/v1/grants", () =>
        json(
            200,
            schema("RoleGrantPage", {
                items: revoked ? [] : [roleGrant],
                next_cursor: null,
            }),
        ),
    ).on("DELETE", "/api/v1/grants/{id}", () => {
        revoked = true;
        return json(200, roleGrant);
    });
    const { user } = start(`/users/${USER_ID}`);
    await user.click(
        await screen.findByRole("button", { name: "Revoke grant" }),
    );
    const revoke = screen.getByRole("dialog");
    expect(revoke).toHaveTextContent("Bob");
    expect(revoke).not.toHaveTextContent(USER_ID);
    await user.click(
        within(revoke).getByRole("button", {
            name: "Confirm revoke grant",
        }),
    );
    expect(
        await screen.findByText("No active grants in this view."),
    ).toBeVisible();
});

it("covers only the authorized scope and never widens a table grant", () => {
    const scoped = session("Administrator", {
        catalog_connection: CATALOG_ID,
        table: null,
    });
    expect(canManageScope(scoped, {})).toBe(false);
    expect(canManageScope(scoped, { catalog_connection: CATALOG_ID })).toBe(
        true,
    );
    expect(canManageScope(scoped, { table: TABLE_ID }, [CATALOG_ID])).toBe(
        true,
    );
    expect(canManageScope(scoped, { table: TABLE_ID }, [USER_ID])).toBe(false);
    expect(
        canManageScope(
            session("Administrator", {
                catalog_connection: null,
                table: TABLE_ID,
            }),
            { catalog_connection: CATALOG_ID },
        ),
    ).toBe(false);
});

it("discards an issued bearer that arrives after sign-out", async () => {
    let deliver: (response: Response) => void = () => {
        throw new Error("issuance has not started");
    };
    fake.on(
        "POST",
        "/api/v1/service-identities/{id}/tokens",
        () =>
            new Promise<Response>((resolve) => {
                deliver = resolve;
            }),
    );
    const { user, client } = start(`/service-identities/${SERVICE_ID}`);
    await user.click(await screen.findByRole("tab", { name: "Tokens" }));
    await user.type(await screen.findByLabelText("Worker pool"), "default");
    await user.click(screen.getByRole("button", { name: "Issue token" }));
    await waitFor(() => {
        expect(
            fake.sent(
                "POST",
                `/api/v1/service-identities/${SERVICE_ID}/tokens`,
            ),
        ).toHaveLength(1);
    });
    fake.on("GET", "/auth/session", () => json(401, {}));
    await act(async () => {
        setSession(client, null);
        deliver(json(201, issuedPoolToken));
        await Promise.resolve();
    });
    expect(
        screen.queryByLabelText("Pool bearer token"),
    ).not.toBeInTheDocument();
    expect(
        JSON.stringify(
            client
                .getQueryCache()
                .getAll()
                .map((query) => query.state.data),
        ),
    ).not.toContain(issuedPoolToken.token);
    expect(client.getMutationCache().getAll()).toHaveLength(0);
});

it("keeps a lost issuance blocked and tells the operator how to recover", async () => {
    fake.on("POST", "/api/v1/service-identities/{id}/tokens", () =>
        json(504, {}),
    );
    const { user } = start(`/service-identities/${SERVICE_ID}`);
    await user.click(await screen.findByRole("tab", { name: "Tokens" }));
    await user.type(await screen.findByLabelText("Worker pool"), "default");
    await user.click(screen.getByRole("button", { name: "Issue token" }));
    await waitFor(() =>
        expect(
            screen.getByRole("button", { name: "Issue token" }),
        ).toBeDisabled(),
    );
    expect(screen.getByText(/reload and revoke/)).toBeVisible();
    expect(
        fake.sent("POST", `/api/v1/service-identities/${SERVICE_ID}/tokens`),
    ).toHaveLength(1);
});

it.each(["grants", "tokens"] as const)(
    "follows %s cursors without widening the resource",
    async (resource) => {
        if (resource === "grants") {
            fake.on("GET", "/api/v1/grants", (request) =>
                json(
                    200,
                    schema("RoleGrantPage", {
                        items: [
                            new URL(request.url).searchParams.has("cursor")
                                ? {
                                      ...roleGrant,
                                      id: TOKEN_ID,
                                      role: "Operator",
                                  }
                                : roleGrant,
                        ],
                        next_cursor: new URL(request.url).searchParams.has(
                            "cursor",
                        )
                            ? null
                            : "next-grants",
                    }),
                ),
            );
        } else {
            fake.on(
                "GET",
                "/api/v1/service-identities/{id}/tokens",
                (request) =>
                    json(
                        200,
                        schema("WorkerPoolTokenPage", {
                            items: [
                                new URL(request.url).searchParams.has("cursor")
                                    ? {
                                          ...poolToken,
                                          id: USER_ID,
                                          label: "Second token",
                                      }
                                    : poolToken,
                            ],
                            next_cursor: new URL(request.url).searchParams.has(
                                "cursor",
                            )
                                ? null
                                : "next-tokens",
                        }),
                    ),
            );
        }
        const { user } = start(
            resource === "grants"
                ? `/users/${USER_ID}`
                : `/service-identities/${SERVICE_ID}`,
        );
        if (resource === "tokens") {
            await user.click(
                await screen.findByRole("tab", { name: "Tokens" }),
            );
        }
        await user.click(
            await screen.findByRole("button", {
                name: `Load more ${resource}`,
            }),
        );
        expect(
            await screen.findByText(
                resource === "grants" ? "Operator" : "Second token",
            ),
        ).toBeVisible();
        const request = fake
            .sent(
                "GET",
                resource === "grants"
                    ? "/api/v1/grants"
                    : `/api/v1/service-identities/${SERVICE_ID}/tokens`,
            )
            .at(-1);
        const params = new URL(request?.url ?? "").searchParams;
        expect(params.get("cursor")).toBe(`next-${resource}`);
        if (resource === "grants")
            expect(params.get("principal_id")).toBe(USER_ID);
    },
);

it("suggests connection names in the grant scope picker", async () => {
    const { user } = start(`/users/${USER_ID}`);
    await user.click(await screen.findByRole("button", { name: "Grant role" }));
    await user.click(screen.getByRole("combobox", { name: "Scope" }));
    await user.click(screen.getByRole("option", { name: "Connection" }));
    expect(
        await screen.findByRole("option", { name: "lake", hidden: true }),
    ).toHaveValue(CATALOG_ID);
    await user.type(screen.getByLabelText("Connection ID"), CATALOG_ID);
    expect(screen.getByRole("button", { name: "Save grant" })).toBeEnabled();
});

it("shows principal and resource names without redundant identifiers", async () => {
    fake.on("GET", "/api/v1/grants", () =>
        json(
            200,
            schema("RoleGrantPage", {
                items: [
                    {
                        ...roleGrant,
                        role: "MaintenanceService",
                        principal_name: "Bob Chen",
                        scope: { table: TABLE_ID },
                        scope_name: "lake.sales.orders",
                    },
                ],
                next_cursor: null,
            }),
        ),
    );
    start(`/users/${USER_ID}`);
    expect(await screen.findByText("Table lake.sales.orders")).toBeVisible();
    const grants = within(screen.getByRole("table"));
    expect(grants.getByText("Bob Chen")).toBeVisible();
    expect(grants.getByText("Maintenance Service")).toBeVisible();
    expect(grants.queryByText(USER_ID)).not.toBeInTheDocument();
    expect(grants.queryByTitle(USER_ID)).not.toBeInTheDocument();
    expect(grants.queryByText(USER_ID.slice(-8))).not.toBeInTheDocument();
});
