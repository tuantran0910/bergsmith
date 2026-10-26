// M4 browser journeys against a contract-typed fake coordinator.

import {
    type QueryClient,
    QueryClientProvider,
    focusManager,
} from "@tanstack/react-query";
import {
    act,
    cleanup,
    render,
    screen,
    waitFor,
    within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { App } from "./App";
import { browser } from "./browser";
import { createQueryClient } from "./queryClient";
import { FakeApi, FakeEventSource, apiError, json, schema } from "./test/fakes";
import {
    CATALOG_ID,
    JOB_ID,
    PRINCIPAL_ID,
    RETRY_JOB_ID,
    TABLE_ID,
    catalogs,
    event,
    job,
    plannedPreview,
    rejectedPreview,
    session,
    table,
    tableSnapshots,
    tableSnapshotsQuiet,
    tableStatistics,
} from "./test/fixtures";

let fake: FakeApi;
let client: QueryClient;

beforeEach(() => {
    fake = new FakeApi();
    vi.stubGlobal("fetch", fake.fetch);
    document.cookie = "bergsmith_csrf=csrf-value";
    fake.on("GET", "/auth/session", () => json(200, session()))
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs))
        // The job page names its table.
        .on("GET", "/api/v1/tables/{id}", () => json(200, table));
});

afterEach(() => {
    expect(fake.unrouted).toEqual([]);
});

function start(path: string) {
    // A previous test's tree unmounts first, so every `start` is fresh.
    cleanup();
    window.history.replaceState(null, "", path);
    const user = userEvent.setup();
    client = createQueryClient(0);
    render(
        <QueryClientProvider client={client}>
            <App />
        </QueryClientProvider>,
    );
    return user;
}

/** The job page's event list. */
function events() {
    return within(screen.getByRole("list", { name: "Job events" }));
}

it("shows ledger statistics on the job detail page", async () => {
    fake.on("GET", "/api/v1/jobs/{id}", () =>
        json(
            200,
            job("Succeeded", {
                statistics: {
                    plan_recorded: true,
                    planned_input_files: "8",
                    planned_input_bytes: "8192",
                    tasks_total: "2",
                    tasks_accepted: "2",
                    attempts: "2",
                    input_files: "8",
                    input_bytes: "8192",
                    output_files: "1",
                    output_bytes: "4096",
                    delete_files_applied: "1",
                    elapsed_millis: "65000",
                    publication_confirmed: true,
                },
            }),
        ),
    );
    start(`/jobs/${JOB_ID}`);
    expect(
        await screen.findByRole("heading", { name: "Maintenance statistics" }),
    ).toBeInTheDocument();
    expect(
        screen.getByText("Input files rewritten").nextElementSibling,
    ).toHaveTextContent("8");
    expect(
        screen.getByText(/Accepted outputs were committed to the table/),
    ).toBeInTheDocument();
});

describe("sign-in", () => {
    it("offers sign-in without a session and returns to the requested page", async () => {
        fake.on("GET", "/auth/session", () =>
            json(401, { error: "no_session" }),
        );
        const assign = vi
            .spyOn(browser, "assign")
            .mockImplementation(() => undefined);
        const user = start("/jobs");

        await user.type(await screen.findByLabelText("Organization"), " acme ");
        await user.click(screen.getByRole("button", { name: "Sign in" }));

        expect(assign).toHaveBeenCalledWith(
            "/auth/login?organization=acme&return_to=%2Fjobs",
        );
    });

    it("returns to sign-in when the session ends while browsing", async () => {
        fake.on("GET", "/api/v1/tables", () =>
            apiError(
                401,
                "authentication_required",
                "PermanentFailure",
                "sign_in",
            ),
        );
        start("/tables");

        expect(
            await screen.findByRole("button", { name: "Sign in" }),
        ).toBeInTheDocument();
    });

    it("signs out with the CSRF token", async () => {
        fake.on("GET", "/api/v1/tables", () =>
            json(200, { items: [], next_cursor: null }),
        ).on("POST", "/auth/logout", () => new Response(null, { status: 204 }));
        const user = start("/tables");

        await user.click(
            await screen.findByRole("button", { name: "Sign out" }),
        );

        expect(
            await screen.findByRole("button", { name: "Sign in" }),
        ).toBeInTheDocument();
        expect(
            fake.sent("POST", "/auth/logout")[0]?.headers.get("x-csrf-token"),
        ).toBe("csrf-value");
    });

    it("explains an unreachable coordinator and retries", async () => {
        fake.on(
            "GET",
            "/auth/session",
            // The first attempt and both automatic retries fail.
            () => Promise.reject(new TypeError("network")),
            () => Promise.reject(new TypeError("network")),
            () => Promise.reject(new TypeError("network")),
            () => json(200, session()),
        );
        fake.on("GET", "/api/v1/tables", () =>
            json(200, { items: [], next_cursor: null }),
        );
        const user = start("/tables");

        expect(
            await screen.findByText("The coordinator could not be reached"),
        ).toBeInTheDocument();
        await user.click(
            screen.getByRole("button", { name: "Retry the same request" }),
        );
        expect(
            await screen.findByText("No tables are visible to you."),
        ).toBeInTheDocument();
    });
});

describe("session identity", () => {
    it("forgets the previous identity's data when another sign-in replaces it", async () => {
        const bob = {
            ...session(),
            principal_id: "0190f3c4-0000-7000-8000-0000000000bb",
            display_name: "Bob",
        };
        let current = session();
        fake.on("GET", "/auth/session", () => json(200, current));
        // Bob's read stalls, so anything on screen must come from the cache.
        fake.on("GET", "/api/v1/jobs", () =>
            current.display_name === "Alice"
                ? json(
                      200,
                      schema("JobPage", {
                          items: [job("Running")],
                          next_cursor: null,
                      }),
                  )
                : new Promise<Response>(() => undefined),
        );
        start("/jobs");
        expect(await screen.findByTitle(JOB_ID)).toBeInTheDocument();

        // Another tab signs in as Bob; returning to this tab re-checks.
        current = bob;
        act(() => {
            focusManager.setFocused(false);
            focusManager.setFocused(true);
        });

        expect(await screen.findByText("Bob")).toBeInTheDocument();
        expect(await screen.findByText("Loading jobs…")).toBeInTheDocument();
        expect(screen.queryByText("Alice")).not.toBeInTheDocument();
        expect(screen.queryByTitle(JOB_ID)).not.toBeInTheDocument();
        focusManager.setFocused(undefined);
    });

    it("resets page state when the identity changes", async () => {
        let current = session();
        fake.on("GET", "/auth/session", () => json(200, current))
            .on("GET", "/api/v1/tables/{id}", () => json(200, table))
            .on("POST", "/api/v1/previews", () => json(200, plannedPreview));
        const user = start(`/tables/${TABLE_ID}`);
        await user.click(
            await screen.findByRole("button", { name: "Preview maintenance" }),
        );
        expect(
            await screen.findByRole("region", { name: "Preview" }),
        ).toBeInTheDocument();

        current = {
            ...session(),
            principal_id: RETRY_JOB_ID,
            display_name: "Bob",
        };
        act(() => {
            focusManager.setFocused(false);
            focusManager.setFocused(true);
        });

        expect(await screen.findByText("Bob")).toBeInTheDocument();
        expect(
            await screen.findByRole("button", { name: "Preview maintenance" }),
        ).toBeInTheDocument();
        expect(
            screen.queryByRole("region", { name: "Preview" }),
        ).not.toBeInTheDocument();
        focusManager.setFocused(undefined);
    });

    it("forgets cached data when the session expires", async () => {
        fake.on("GET", "/api/v1/jobs", () =>
            json(
                200,
                schema("JobPage", {
                    items: [job("Running")],
                    next_cursor: null,
                }),
            ),
        );
        fake.on("GET", "/api/v1/jobs/{id}", () =>
            apiError(
                401,
                "authentication_required",
                "PermanentFailure",
                "sign_in",
            ),
        );
        const user = start("/jobs");
        await user.click(await screen.findByTitle(JOB_ID));

        expect(
            await screen.findByRole("button", { name: "Sign in" }),
        ).toBeInTheDocument();
        // Nothing read under the expired session keeps its data.
        expect(client.getQueryData(["jobs"])).toBeUndefined();
        expect(
            client
                .getQueryCache()
                .getAll()
                .filter((query) => query.state.data !== undefined)
                .map((query) => query.queryKey[0]),
        ).toEqual(["session"]);
    });
});

describe("browsing", () => {
    it("opens a table's statistics tab and keeps it in the URL", async () => {
        fake.on("GET", "/api/v1/tables/{id}/statistics", () =>
            json(200, tableStatistics),
        );
        const user = start(`/tables/${TABLE_ID}`);

        await user.click(
            await screen.findByRole("tab", { name: "Statistics" }),
        );

        expect(window.location.search).toBe("?tab=statistics");
        expect(
            await screen.findByRole("img", { name: /Data file sizes/ }),
        ).toBeInTheDocument();
        expect(
            screen.getByText("+2.0 KiB since the previous sample"),
        ).toBeInTheDocument();
        expect(screen.getByText("day=2026-10-01")).toBeInTheDocument();
        expect(screen.getAllByText("200").length).toBeGreaterThan(0);
        expect(screen.getAllByText("120").length).toBeGreaterThan(0);
        const partitions = screen.getByRole("region", {
            name: "Busiest partitions",
        });
        expect(
            within(partitions).getByRole("cell", { name: "1" }),
        ).toBeInTheDocument();

        await user.click(screen.getByRole("tab", { name: "Overview" }));
        expect(window.location.search).toBe("");
        expect(
            screen.getByRole("button", { name: "Preview maintenance" }),
        ).toBeInTheDocument();
    });

    it("opens a table's snapshot timeline and links its publications", async () => {
        fake.on("GET", "/api/v1/tables/{id}/snapshots", () =>
            json(200, tableSnapshots),
        );
        const user = start(`/tables/${TABLE_ID}`);

        await user.click(await screen.findByRole("tab", { name: "Snapshots" }));

        expect(window.location.search).toBe("?tab=snapshots");
        expect(
            await screen.findByRole("img", { name: /Commits per hour/ }),
        ).toBeInTheDocument();
        expect(screen.getByText("+2 data files, +40 rows")).toBeInTheDocument();
        expect(
            screen.getByRole("list", { name: "Commits" }),
        ).toBeInTheDocument();
        // A Bergsmith publication links to the job that made it.
        expect(
            screen.getByRole("link", { name: "job 0000000a" }),
        ).toHaveAttribute("href", `/jobs/${JOB_ID}`);
        // A commit from another engine names the engine instead.
        expect(
            screen.getByText("Committed by spark 4.0.0"),
        ).toBeInTheDocument();

        // The request asked for the default day of cadence buckets.
        const requested = fake.sent(
            "GET",
            `/api/v1/tables/${TABLE_ID}/snapshots`,
        )[0];
        expect(requested && new URL(requested.url).search).toBe(
            "?limit=50&hours=24",
        );

        await user.click(screen.getByRole("tab", { name: "Overview" }));
        expect(window.location.search).toBe("");
    });

    it("shows an empty cadence without inventing a peak", async () => {
        fake.on("GET", "/api/v1/tables/{id}/snapshots", () =>
            json(200, tableSnapshotsQuiet),
        );
        const user = start(`/tables/${TABLE_ID}`);

        await user.click(await screen.findByRole("tab", { name: "Snapshots" }));

        expect(
            await screen.findByText("No commits in the last 24 hours."),
        ).toBeInTheDocument();
        const chart = screen.getByRole("img", { name: /Commits per hour/ });
        const label = chart.getAttribute("aria-label") ?? "";
        expect(label).toContain("No commits in the last 24 hours.");
        expect(label).not.toMatch(/NaN|at most 1 commit|peaking/);
        expect(screen.getByText("No commits recorded yet")).toBeInTheDocument();
    });

    it("lists tables by alias and pages with the cursor", async () => {
        fake.on(
            "GET",
            "/api/v1/tables",
            () =>
                json(
                    200,
                    schema("TablePage", {
                        items: [table],
                        next_cursor: "next-page",
                    }),
                ),
            () =>
                json(
                    200,
                    schema("TablePage", {
                        items: [
                            {
                                ...table,
                                id: "0190f3c4-0000-7000-8000-000000000002",
                                aliases: [],
                            },
                        ],
                        next_cursor: null,
                    }),
                ),
        );
        const user = start("/tables");
        expect(window.location.pathname).toBe("/tables");

        expect(
            await screen.findByRole("link", { name: /^orders/ }),
        ).toHaveAttribute("href", `/tables/${TABLE_ID}`);
        expect(screen.getByRole("link", { name: "Tables" })).toHaveAttribute(
            "aria-current",
            "page",
        );
        expect(
            screen.getByTitle("Also lake.legacy.orders_v1"),
        ).toBeInTheDocument();
        await user.click(
            screen.getByRole("button", { name: "Load more tables" }),
        );

        expect(
            await screen.findByRole("link", {
                name: "00000002",
            }),
        ).toHaveAttribute("title", "0190f3c4-0000-7000-8000-000000000002");
        expect(
            new URL(
                fake.sent("GET", "/api/v1/tables")[1]?.url ?? "",
            ).searchParams.get("cursor"),
        ).toBe("next-page");
        expect(
            screen.queryByRole("button", { name: "Load more tables" }),
        ).not.toBeInTheDocument();
    });

    it("copies a table's full UUID from its details page", async () => {
        fake.on("GET", "/api/v1/tables", () =>
            json(200, { items: [table], next_cursor: null }),
        ).on("GET", "/api/v1/tables/{id}", () => json(200, table));
        const user = start("/tables");

        await user.click(await screen.findByRole("link", { name: /^orders/ }));
        expect(await screen.findByText(table.table_uuid)).toBeInTheDocument();
        await user.click(
            screen.getByRole("button", { name: "Copy table UUID" }),
        );

        expect(await navigator.clipboard.readText()).toBe(table.table_uuid);
        expect(screen.getByText("Copied table UUID")).toBeInTheDocument();
    });

    it("opens a table from the list", async () => {
        fake.on("GET", "/api/v1/tables", () =>
            json(200, { items: [table], next_cursor: null }),
        ).on("GET", "/api/v1/tables/{id}", () => json(200, table));
        const user = start("/tables");

        await user.click(await screen.findByRole("link", { name: /^orders/ }));

        expect(
            await screen.findByRole("heading", { name: "lake.sales.orders" }),
        ).toBeInTheDocument();
        expect(window.location.pathname).toBe(`/tables/${TABLE_ID}`);
    });

    it("filters tables by name and keeps the filter in the address", async () => {
        const other = schema("Table", {
            ...table,
            id: "0190f3c4-0000-7000-8000-000000000002",
            table_uuid: "6f1c0d0e-1111-4222-8333-444455556667",
            created_at: "2026-10-05T00:00:00Z",
            aliases: [
                {
                    catalog_id: CATALOG_ID,
                    namespace: ["marketing"],
                    name: "events",
                    revision: 1,
                },
            ],
        });
        fake.on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", {
                    items: [table, other],
                    next_cursor: null,
                }),
            ),
        );
        const user = start("/tables");

        expect(
            await screen.findByRole("link", { name: /^orders/ }),
        ).toBeInTheDocument();
        expect(
            screen.getByRole("link", { name: /^events/ }),
        ).toBeInTheDocument();

        await user.type(
            screen.getByRole("searchbox", { name: "Search tables" }),
            "events",
        );

        await waitFor(() => {
            expect(
                screen.queryByRole("link", { name: /^orders/ }),
            ).not.toBeInTheDocument();
        });
        expect(
            screen.getByRole("link", { name: /^events/ }),
        ).toBeInTheDocument();
        expect(new URL(window.location.href).searchParams.get("q")).toBe(
            "events",
        );
        expect(screen.getByText("1 of 2 tables")).toBeInTheDocument();

        await user.click(screen.getByRole("button", { name: "Clear filters" }));
        expect(
            await screen.findByRole("link", { name: /^orders/ }),
        ).toBeInTheDocument();
        expect(new URL(window.location.href).searchParams.get("q")).toBeNull();
    });

    it("explains no matches and clears the filters", async () => {
        fake.on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        );
        const user = start("/tables");
        await screen.findByRole("link", { name: /^orders/ });

        await user.type(
            screen.getByRole("searchbox", { name: "Search tables" }),
            "missing",
        );

        expect(await screen.findByText("No matches.")).toBeInTheDocument();
        expect(
            screen.getByText(/No tables match these filters/),
        ).toBeInTheDocument();
        // The empty state owns the only clear action, so the two never
        // compete above an empty grid.
        await user.click(screen.getByRole("button", { name: "Clear filters" }));
        expect(
            await screen.findByRole("link", { name: /^orders/ }),
        ).toBeInTheDocument();
    });

    it("finds an unnamed table by its short ID", async () => {
        const unnamed = schema("Table", {
            ...table,
            id: "0190f3c4-0000-7000-8000-000000000002",
            table_uuid: "6f1c0d0e-1111-4222-8333-444455556667",
            aliases: [],
        });
        fake.on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", {
                    items: [table, unnamed],
                    next_cursor: null,
                }),
            ),
        );
        const user = start("/tables");
        await screen.findByRole("link", { name: /^orders/ });

        await user.type(
            screen.getByRole("searchbox", { name: "Search tables" }),
            "00000002",
        );

        expect(
            await screen.findByRole("link", { name: "00000002" }),
        ).toBeInTheDocument();
        expect(
            screen.queryByRole("link", { name: /^orders/ }),
        ).not.toBeInTheDocument();
    });

    it("keeps loading more available when a filter has no matches", async () => {
        const other = schema("Table", {
            ...table,
            id: "0190f3c4-0000-7000-8000-000000000004",
            table_uuid: "6f1c0d0e-1111-4222-8333-444455556668",
            created_at: "2026-10-05T00:00:00Z",
            aliases: [
                {
                    catalog_id: CATALOG_ID,
                    namespace: ["marketing"],
                    name: "events",
                    revision: 1,
                },
            ],
        });
        fake.on(
            "GET",
            "/api/v1/tables",
            () =>
                json(
                    200,
                    schema("TablePage", {
                        items: [table],
                        next_cursor: "next-page",
                    }),
                ),
            () =>
                json(
                    200,
                    schema("TablePage", {
                        items: [other],
                        next_cursor: null,
                    }),
                ),
        );
        const user = start("/tables");
        await screen.findByRole("link", { name: /^orders/ });

        await user.type(
            screen.getByRole("searchbox", { name: "Search tables" }),
            "events",
        );

        expect(await screen.findByText("No matches.")).toBeInTheDocument();
        expect(
            screen.getByText(/cover the 1 loaded table/),
        ).toBeInTheDocument();
        await user.click(
            screen.getByRole("button", { name: "Load more tables" }),
        );

        expect(
            await screen.findByRole("link", { name: /^events/ }),
        ).toBeInTheDocument();
        expect(
            new URL(
                fake.sent("GET", "/api/v1/tables")[1]?.url ?? "",
            ).searchParams.get("cursor"),
        ).toBe("next-page");
    });

    it("sorts by registered date through the address", async () => {
        const other = schema("Table", {
            ...table,
            id: "0190f3c4-0000-7000-8000-000000000003",
            table_uuid: "6f1c0d0e-1111-4222-8333-444455556668",
            created_at: "2026-10-05T00:00:00Z",
            aliases: [
                {
                    catalog_id: CATALOG_ID,
                    namespace: ["marketing"],
                    name: "events",
                    revision: 1,
                },
            ],
        });
        fake.on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", {
                    items: [other, table],
                    next_cursor: null,
                }),
            ),
        );
        const user = start("/tables");
        await screen.findByRole("link", { name: /^orders/ });

        const header = screen.getByRole("columnheader", {
            name: /Registered/,
        });
        expect(header).toHaveAttribute("aria-sort", "none");
        await user.click(
            within(header).getByRole("button", { name: /Registered/ }),
        );

        await waitFor(() => {
            expect(header).toHaveAttribute("aria-sort", "ascending");
        });
        expect(new URL(window.location.href).searchParams.get("sort")).toBe(
            "registered",
        );
        const rows = screen.getAllByRole("row").slice(1);
        expect(
            within(rows[0] as HTMLElement).getByRole("link", {
                name: /^orders/,
            }),
        ).toBeInTheDocument();

        await user.click(
            within(header).getByRole("button", { name: /Registered/ }),
        );
        await waitFor(() => {
            expect(header).toHaveAttribute("aria-sort", "descending");
        });
        expect(new URL(window.location.href).searchParams.get("order")).toBe(
            "desc",
        );
        const ordered = screen.getAllByRole("row").slice(1);
        expect(
            within(ordered[0] as HTMLElement).getByRole("link", {
                name: /^events/,
            }),
        ).toBeInTheDocument();
    });

    it("omits an unknown catalog name instead of showing its ID", async () => {
        fake.on("GET", "/api/v1/catalogs", () =>
            json(200, { items: [], next_cursor: null }),
        ).on("GET", "/api/v1/tables/{id}", () => json(200, table));
        start(`/tables/${TABLE_ID}`);

        expect(
            await screen.findByRole("heading", { name: "sales.orders" }),
        ).toBeInTheDocument();
    });

    it("shows a typed not-found error for an inaccessible table", async () => {
        fake.on("GET", "/api/v1/tables/{id}", () =>
            apiError(
                404,
                "resource_not_found",
                "PermanentFailure",
                "check_resource_and_grants",
            ),
        );
        start(`/tables/${TABLE_ID}`);

        expect(
            await screen.findByText("Resource not found"),
        ).toBeInTheDocument();
        expect(
            screen.getByText("Next step: Check resource and grants."),
        ).toBeInTheDocument();
        expect(
            screen.queryByRole("button", { name: "Retry the same request" }),
        ).not.toBeInTheDocument();
    });

    it.each(["/nowhere", `/jobs/${JOB_ID}/extra`, "/tables/not-a-uuid"])(
        "reports %s as a page that does not exist, inside the shell",
        async (path) => {
            start(path);
            expect(
                await screen.findByText("This page does not exist."),
            ).toBeInTheDocument();
            expect(
                screen.getByRole("navigation", { name: "Main" }),
            ).toBeInTheDocument();
        },
    );
});

describe("overview and shell", () => {
    it("lands on the overview with quick links and honest placeholders", async () => {
        start("/");

        expect(
            await screen.findByRole("heading", { name: "Overview" }),
        ).toBeInTheDocument();
        expect(
            screen.getByRole("link", { name: /Browse tables and start/ }),
        ).toHaveAttribute("href", "/tables");
        expect(
            screen.getByRole("link", { name: /Watch maintenance run/ }),
        ).toHaveAttribute("href", "/jobs");
        expect(
            screen.getByText("Attention overview is not yet available"),
        ).toBeInTheDocument();
        expect(
            screen.queryByRole("link", { name: /Browse tables/ }),
        ).not.toHaveAttribute("aria-current", "page");
        expect(screen.getByRole("link", { name: "Overview" })).toHaveAttribute(
            "aria-current",
            "page",
        );
    });

    it("reaches the tables quickly and the rail keeps its groups", async () => {
        fake.on("GET", "/auth/session", () =>
            json(200, session("Administrator")),
        ).on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        );
        start("/");
        const user = userEvent.setup();
        await user.click(
            await screen.findByRole("link", {
                name: /Browse tables and start/,
            }),
        );

        expect(
            await screen.findByRole("link", { name: /^orders/ }),
        ).toBeInTheDocument();
        expect(
            within(screen.getByRole("navigation", { name: "Main" })).getByText(
                "Data",
            ),
        ).toBeInTheDocument();
        expect(
            within(screen.getByRole("navigation", { name: "Main" })).getByText(
                "Operations",
            ),
        ).toBeInTheDocument();
        expect(
            within(screen.getByRole("navigation", { name: "Main" })).getByText(
                "Administration",
            ),
        ).toBeInTheDocument();
    });

    it("hides the administration rail group without the permission", async () => {
        fake.on("GET", "/auth/session", () => json(200, session("Viewer")));
        fake.on("GET", "/api/v1/tables", () =>
            json(200, schema("TablePage", { items: [], next_cursor: null })),
        );
        start("/tables");

        const nav = await screen.findByRole("navigation", { name: "Main" });
        expect(within(nav).getByText("Data")).toBeInTheDocument();
        expect(within(nav).getByText("Operations")).toBeInTheDocument();
        expect(
            within(nav).queryByText("Administration"),
        ).not.toBeInTheDocument();
    });

    it("moves through the navigation drawer in the narrow layout", async () => {
        fake.on("GET", "/api/v1/tables", () =>
            json(200, schema("TablePage", { items: [], next_cursor: null })),
        );
        fake.on("GET", "/api/v1/jobs", () =>
            json(200, schema("JobPage", { items: [], next_cursor: null })),
        );
        const user = start("/tables");
        await user.click(
            await screen.findByRole("button", { name: "Open navigation" }),
        );

        const drawer = await screen.findByRole("dialog");
        expect(
            within(drawer).getByRole("link", { name: "Tables" }),
        ).toHaveAttribute("aria-current", "page");
        await user.click(within(drawer).getByRole("link", { name: "Jobs" }));

        expect(
            await screen.findByRole("heading", { name: "Jobs" }),
        ).toBeInTheDocument();
        await waitFor(() => {
            expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
        });
        // The new page owns focus instead of the drawer trigger.
        expect(document.getElementById("main")).toHaveFocus();
    });

    it("names the skip link target", async () => {
        fake.on("GET", "/api/v1/tables", () =>
            json(200, schema("TablePage", { items: [], next_cursor: null })),
        );
        start("/tables");
        expect(
            await screen.findByRole("link", { name: "Skip to main content" }),
        ).toHaveAttribute("href", "#main");
    });

    it("locates a resource page with breadcrumbs", async () => {
        fake.on("GET", "/api/v1/tables/{id}", () => json(200, table));
        start(`/tables/${TABLE_ID}`);

        const trail = await screen.findByRole("navigation", {
            name: "Breadcrumb",
        });
        expect(
            within(trail).getByRole("link", { name: "Overview" }),
        ).toHaveAttribute("href", "/");
        expect(
            within(trail).getByRole("link", { name: "Tables" }),
        ).toHaveAttribute("href", "/tables");
        expect(
            await within(trail).findByText("lake.sales.orders"),
        ).toHaveAttribute("title", TABLE_ID);
        expect(within(trail).queryByText("00000001")).not.toBeInTheDocument();
    });

    it("replaces a detail breadcrumb after navigating to another resource", async () => {
        fake.on("GET", "/api/v1/tables/{id}", () => json(200, table))
            .on("GET", "/api/v1/jobs", () =>
                json(
                    200,
                    schema("JobPage", {
                        items: [job("Running")],
                        next_cursor: null,
                    }),
                ),
            )
            .on("GET", "/api/v1/jobs/{id}", () => json(200, job("Running")));
        const user = start(`/tables/${TABLE_ID}`);

        let trail = await screen.findByRole("navigation", {
            name: "Breadcrumb",
        });
        expect(
            await within(trail).findByText("lake.sales.orders"),
        ).toBeInTheDocument();

        await user.click(screen.getByRole("link", { name: "Jobs" }));
        await user.click(
            await screen.findByRole("link", {
                name: `Job ${JOB_ID.slice(-8)}`,
            }),
        );

        trail = await screen.findByRole("navigation", { name: "Breadcrumb" });
        expect(
            await within(trail).findByText(`Job ${JOB_ID.slice(-8)}`),
        ).toBeInTheDocument();
        expect(
            within(trail).queryByText("lake.sales.orders"),
        ).not.toBeInTheDocument();
    });
});

describe("preview and run", () => {
    beforeEach(() => {
        fake.on("GET", "/api/v1/tables/{id}", () => json(200, table));
    });

    it("previews without precision loss and shows the estimate", async () => {
        fake.on("POST", "/api/v1/previews", () => json(200, plannedPreview));
        const user = start(`/tables/${TABLE_ID}`);

        await user.click(
            await screen.findByRole("button", { name: "Preview maintenance" }),
        );

        const preview = await screen.findByRole("region", { name: "Preview" });
        expect(
            within(preview).getByText("Maintenance would run"),
        ).toBeInTheDocument();
        expect(
            within(preview).getByText("9007199254740993"),
        ).toBeInTheDocument();
        expect(within(preview).getByText("1 min 35 s")).toBeInTheDocument();
        expect(
            within(preview).getByText("Showing 1 of 4 bins."),
        ).toBeInTheDocument();
        expect(
            within(preview).getByText("At target size: 3 files, 1.5 GiB"),
        ).toBeInTheDocument();
        const request = fake.sent("POST", "/api/v1/previews")[0];
        expect(request?.headers.get("x-csrf-token")).toBe("csrf-value");
        expect(await request?.json()).toEqual({ table_id: TABLE_ID });
    });

    it("gates preview and submission on the table's grant coverage", async () => {
        const otherTable = "0190f3c4-0000-7000-8000-000000000002";
        const otherConnection = "0190f3c4-0000-7000-8000-0000000000c2";
        for (const current of [
            session("Viewer"),
            session("Operator", {
                catalog_connection: null,
                table: otherTable,
            }),
            session("Operator", {
                catalog_connection: otherConnection,
                table: null,
            }),
        ]) {
            fake.on("GET", "/auth/session", () => json(200, current));
            start(`/tables/${TABLE_ID}`);
            expect(await screen.findByText("Table UUID")).toBeInTheDocument();
            expect(
                screen.queryByRole("button", { name: "Preview maintenance" }),
            ).not.toBeInTheDocument();
            expect(
                screen.queryByRole("button", { name: "Run maintenance" }),
            ).not.toBeInTheDocument();
            cleanup();
        }

        // A grant over a connection the table is named on covers the table.
        fake.on("GET", "/auth/session", () =>
            json(
                200,
                session("Operator", {
                    catalog_connection: CATALOG_ID,
                    table: null,
                }),
            ),
        );
        start(`/tables/${TABLE_ID}`);
        expect(
            await screen.findByRole("button", { name: "Preview maintenance" }),
        ).toBeInTheDocument();
        expect(
            screen.getByRole("button", { name: "Run maintenance" }),
        ).toBeInTheDocument();
    });

    it("refreshes maintenance actions when the session's permissions change", async () => {
        let current = session("Operator");
        fake.on("GET", "/auth/session", () => json(200, current));
        start(`/tables/${TABLE_ID}`);
        expect(
            await screen.findByRole("button", { name: "Run maintenance" }),
        ).toBeInTheDocument();

        // A revoked permission removes the actions without leaving the page.
        current = session("Viewer");
        act(() => {
            focusManager.setFocused(false);
            focusManager.setFocused(true);
        });
        await waitFor(() => {
            expect(
                screen.queryByRole("button", { name: "Run maintenance" }),
            ).not.toBeInTheDocument();
        });
        expect(
            screen.queryByRole("button", { name: "Preview maintenance" }),
        ).not.toBeInTheDocument();

        // A new grant brings them back in place.
        current = session("Operator");
        act(() => {
            focusManager.setFocused(false);
            focusManager.setFocused(true);
        });
        expect(
            await screen.findByRole("button", { name: "Run maintenance" }),
        ).toBeInTheDocument();
        focusManager.setFocused(undefined);
    });

    it("replaces every bin when a preview is repeated", async () => {
        const partition = (value: string) => ({
            partition: { spec_id: 0, values: [{ field_id: 1000, value }] },
            ordinal: 0,
            input: { files: 2, bytes: 2048 },
            has_deletes: false,
            files_removed: 1,
            disposition: "admitted" as const,
        });
        const errors = vi.spyOn(console, "error");
        fake.on(
            "POST",
            "/api/v1/previews",
            () =>
                json(200, {
                    ...plannedPreview,
                    bins: [partition("2026-10-19"), partition("2026-10-20")],
                    bins_total: 2,
                    bins_truncated: false,
                }),
            () =>
                json(200, {
                    ...plannedPreview,
                    bins: [partition("2026-10-21")],
                    bins_total: 1,
                    bins_truncated: false,
                }),
        );
        const user = start(`/tables/${TABLE_ID}`);
        const button = await screen.findByRole("button", {
            name: "Preview maintenance",
        });

        await user.click(button);
        expect(await screen.findByText(/2026-10-20/)).toBeInTheDocument();
        await user.click(button);
        expect(await screen.findByText(/2026-10-21/)).toBeInTheDocument();

        const preview = screen.getByRole("region", { name: "Preview" });
        // One header row and one bin row.
        expect(within(preview).getAllByRole("row")).toHaveLength(2);
        expect(
            within(preview).queryByText(/2026-10-19/),
        ).not.toBeInTheDocument();
        expect(
            errors.mock.calls.some((call) =>
                String(call[0]).includes("same key"),
            ),
        ).toBe(false);
    });

    it("shows why a table is unsupported", async () => {
        fake.on("POST", "/api/v1/previews", () => json(200, rejectedPreview));
        const user = start(`/tables/${TABLE_ID}`);

        await user.click(
            await screen.findByRole("button", { name: "Preview maintenance" }),
        );

        expect(
            await screen.findByText(
                "Unsupported: Bergsmith will not rewrite this table",
            ),
        ).toBeInTheDocument();
        expect(screen.getByText("Sorted table")).toBeInTheDocument();
    });

    it("explains a no-op preview", async () => {
        fake.on("POST", "/api/v1/previews", () =>
            json(200, {
                ...plannedPreview,
                outcome: "no_op",
                no_op_reason: "no_snapshot",
                baseline: null,
            }),
        );
        const user = start(`/tables/${TABLE_ID}`);

        await user.click(
            await screen.findByRole("button", { name: "Preview maintenance" }),
        );

        expect(
            await screen.findByText("Nothing to do: the table has no snapshot"),
        ).toBeInTheDocument();
    });

    it("retries a transient preview failure", async () => {
        fake.on(
            "POST",
            "/api/v1/previews",
            () =>
                apiError(
                    503,
                    "preview_capacity_exhausted",
                    "RetrySameRequest",
                    "retry_same_request",
                ),
            () => json(200, plannedPreview),
        );
        const user = start(`/tables/${TABLE_ID}`);

        await user.click(
            await screen.findByRole("button", { name: "Preview maintenance" }),
        );
        await user.click(
            await screen.findByRole("button", {
                name: "Retry the same request",
            }),
        );

        expect(
            await screen.findByText("Maintenance would run"),
        ).toBeInTheDocument();
    });

    it("submits once per action, replaying the key after a transient failure", async () => {
        fake.on(
            "POST",
            "/api/v1/jobs",
            () =>
                apiError(
                    503,
                    "ledger_unavailable",
                    "RetrySameRequest",
                    "retry_same_request",
                ),
            () =>
                json(
                    202,
                    schema("AcceptedJob", {
                        operation_id: JOB_ID,
                        job_id: JOB_ID,
                        resource_url: `/api/v1/jobs/${JOB_ID}`,
                    }),
                ),
        ).on("GET", "/api/v1/jobs/{id}", () => json(200, job("Planning")));
        const user = start(`/tables/${TABLE_ID}`);

        await user.click(
            await screen.findByRole("button", { name: "Run maintenance" }),
        );
        expect(
            await screen.findByText("Ledger unavailable"),
        ).toBeInTheDocument();
        await user.click(
            screen.getByRole("button", { name: "Retry the same request" }),
        );

        expect(await screen.findByText(JOB_ID)).toBeInTheDocument();
        expect(window.location.pathname).toBe(`/jobs/${JOB_ID}`);
        const [first, second] = fake.sent("POST", "/api/v1/jobs");
        const key = first?.headers.get("Idempotency-Key");
        expect(key).toMatch(/^[0-9a-f-]{36}$/);
        expect(second?.headers.get("Idempotency-Key")).toBe(key);
        expect(second?.headers.get("x-csrf-token")).toBe("csrf-value");
        expect(await second?.json()).toEqual({ table_id: TABLE_ID });
    });

    it.each([
        [504, true],
        [500, false],
    ])(
        "keeps the key after an untyped %d, whose outcome is unknown",
        async (status, offersRetry) => {
            fake.on(
                "POST",
                "/api/v1/jobs",
                () => new Response("upstream timed out", { status }),
                () =>
                    json(
                        202,
                        schema("AcceptedJob", {
                            operation_id: JOB_ID,
                            job_id: JOB_ID,
                            resource_url: `/api/v1/jobs/${JOB_ID}`,
                        }),
                    ),
            ).on("GET", "/api/v1/jobs/{id}", () => json(200, job("Planning")));
            const user = start(`/tables/${TABLE_ID}`);

            await user.click(
                await screen.findByRole("button", { name: "Run maintenance" }),
            );
            expect(
                await screen.findByText(`Request failed (${status})`),
            ).toBeInTheDocument();
            const retry = screen.queryByRole("button", {
                name: "Retry the same request",
            });
            expect(retry !== null).toBe(offersRetry);
            await user.click(
                retry ??
                    screen.getByRole("button", { name: "Run maintenance" }),
            );

            expect(await screen.findByText(JOB_ID)).toBeInTheDocument();
            const [first, second] = fake.sent("POST", "/api/v1/jobs");
            expect(second?.headers.get("Idempotency-Key")).toBe(
                first?.headers.get("Idempotency-Key"),
            );
        },
    );

    it("uses a new key after a permanent failure", async () => {
        fake.on("POST", "/api/v1/jobs", () =>
            apiError(
                403,
                "permission_denied",
                "PermanentFailure",
                "request_permission",
            ),
        );
        const user = start(`/tables/${TABLE_ID}`);

        await user.click(
            await screen.findByRole("button", { name: "Run maintenance" }),
        );
        expect(
            await screen.findByText("Permission denied"),
        ).toBeInTheDocument();
        expect(
            screen.queryByRole("button", { name: "Retry the same request" }),
        ).not.toBeInTheDocument();
        await user.click(
            screen.getByRole("button", { name: "Run maintenance" }),
        );

        await waitFor(() => {
            expect(fake.sent("POST", "/api/v1/jobs")).toHaveLength(2);
        });
        const [first, second] = fake.sent("POST", "/api/v1/jobs");
        expect(second?.headers.get("Idempotency-Key")).not.toBe(
            first?.headers.get("Idempotency-Key"),
        );
    });
});

describe("job progress", () => {
    it("separates execution from publication as events arrive, then closes", async () => {
        // The job resource follows the events, as the ledger would.
        let current = job("Running");
        fake.on("GET", "/api/v1/jobs/{id}", () => json(200, current));
        start(`/jobs/${JOB_ID}`);

        expect(
            await screen.findByText("Running", {
                selector: '[data-slot="badge"]',
            }),
        ).toBeInTheDocument();
        expect(screen.getByText("Not started")).toBeInTheDocument();
        const stream = FakeEventSource.latest();
        expect(stream.url).toBe(`/api/v1/jobs/${JOB_ID}/events`);
        act(() => {
            stream.open();
        });
        expect(screen.getByText("Live.")).toBeInTheDocument();

        current = job("ReadyToCommit", { publication_started: true });
        act(() => {
            stream.emit(event(1, "Planning", null));
            stream.emit(event(2, "ReadyToCommit", "Running"));
            stream.emit(event(2, "ReadyToCommit", "Running"));
        });
        expect(await screen.findByText("Outputs ready")).toBeInTheDocument();
        expect(screen.getByText("Commit authorized")).toBeInTheDocument();
        expect(events().getAllByRole("listitem")).toHaveLength(2);

        current = job("Succeeded", { publication_started: true });
        act(() => {
            stream.emit(event(3, "Succeeded", "Committing"));
        });
        expect(await screen.findByText("Confirmed")).toBeInTheDocument();
        expect(stream.readyState).toBe(FakeEventSource.CLOSED);
        expect(screen.getByText("The job has ended.")).toBeInTheDocument();
        expect(
            screen.queryByRole("button", { name: "Cancel job" }),
        ).not.toBeInTheDocument();
        expect(
            screen.queryByRole("button", { name: "Retry with a fresh plan" }),
        ).not.toBeInTheDocument();
    });

    it("names the job's table and requester, or shows the table's ID without a visible name", async () => {
        fake.on("GET", "/api/v1/jobs/{id}", () => json(200, job("Running")));
        start(`/jobs/${JOB_ID}`);

        expect(
            await screen.findByRole("link", { name: "lake.sales.orders" }),
        ).toHaveAttribute("href", `/tables/${TABLE_ID}`);
        expect(
            screen.getByRole("heading", { name: "lake.sales.orders" }),
        ).toBeInTheDocument();
        expect(
            screen.getByRole("button", { name: "Copy job ID" }),
        ).toBeInTheDocument();
        const technical = screen
            .getByText("Technical details")
            .closest("details");
        expect(technical).not.toBeNull();
        if (technical !== null) {
            expect(technical).not.toHaveAttribute("open");
            expect(within(technical).getByText(JOB_ID)).toBeInTheDocument();
        }
        expect(
            screen.getByText("Requested by").nextElementSibling,
        ).toHaveTextContent("Alice");
        cleanup();

        fake.on("GET", "/api/v1/jobs/{id}", () =>
            json(200, job("Running", { table_aliases: [] })),
        );
        start(`/jobs/${JOB_ID}`);
        expect(
            await screen.findByRole("heading", {
                name: `Table ${TABLE_ID.slice(-8)}`,
            }),
        ).toBeInTheDocument();
        expect(
            await screen.findByRole("link", {
                name: `Table ${TABLE_ID.slice(-8)}`,
            }),
        ).toHaveAttribute("href", `/tables/${TABLE_ID}`);
    });

    it("coalesces refreshes while replaying many events", async () => {
        let current = job("Running");
        fake.on("GET", "/api/v1/jobs/{id}", () => json(200, current));
        start(`/jobs/${JOB_ID}`);
        await screen.findByText("Running", { selector: '[data-slot="badge"]' });
        const stream = FakeEventSource.latest();

        current = job("ReadyToCommit", { publication_started: true });
        act(() => {
            for (let sequence = 1; sequence <= 100; sequence += 1) {
                stream.emit(event(sequence, "Running", "Queued"));
            }
        });

        // The last read reflects the newest event.
        expect(
            await screen.findByText("Commit authorized"),
        ).toBeInTheDocument();
        expect(events().getAllByRole("listitem")).toHaveLength(100);
        // The initial read plus at most one running and one trailing refresh.
        expect(
            fake.sent("GET", `/api/v1/jobs/${JOB_ID}`).length,
        ).toBeLessThanOrEqual(3);
    });

    it("aborts a superseded job read", async () => {
        const signals: AbortSignal[] = [];
        fake.on(
            "GET",
            "/api/v1/jobs/{id}",
            () => json(200, job("Running")),
            (request) => {
                signals.push(request.signal);
                return new Promise<Response>(() => undefined);
            },
            () => json(200, job("Queued")),
        );
        start(`/jobs/${JOB_ID}`);
        await screen.findByText("Running", { selector: '[data-slot="badge"]' });

        // A read stalls; a newer read of the same job replaces it.
        void client.refetchQueries({ queryKey: ["job", JOB_ID] });
        await waitFor(() => {
            expect(signals).toHaveLength(1);
        });
        await act(() => client.refetchQueries({ queryKey: ["job", JOB_ID] }));

        expect(signals[0]?.aborted).toBe(true);
        expect(
            await screen.findByText("Queued", {
                selector: '[data-slot="badge"]',
            }),
        ).toBeInTheDocument();
    });

    it("stops a terminal job's stream from reconnecting forever", async () => {
        fake.on("GET", "/api/v1/jobs/{id}", () => json(200, job("Failed")));
        start(`/jobs/${JOB_ID}`);
        await screen.findByText("Failed", { selector: '[data-slot="badge"]' });
        const stream = FakeEventSource.latest();

        act(() => {
            stream.drop();
        });

        await waitFor(() => {
            expect(stream.readyState).toBe(FakeEventSource.CLOSED);
        });
    });

    it("resumes after the last event when the user reconnects", async () => {
        fake.on("GET", "/api/v1/jobs/{id}", () => json(200, job("Running")));
        const user = start(`/jobs/${JOB_ID}`);
        await screen.findByText("Running", { selector: '[data-slot="badge"]' });
        const first = FakeEventSource.latest();
        act(() => {
            first.emit(event(4, "Running", "Queued"));
            first.emit({
                ...event(5, "Running", "Running"),
                kind: "cancellation_requested",
            });
        });

        act(() => {
            first.drop();
        });
        expect(await screen.findByText("Reconnecting…")).toBeInTheDocument();
        act(() => {
            first.fail();
        });
        await user.click(
            await screen.findByRole("button", { name: "Reconnect" }),
        );

        expect(FakeEventSource.latest().url).toBe(
            `/api/v1/jobs/${JOB_ID}/events?after=5`,
        );
        expect(first.readyState).toBe(FakeEventSource.CLOSED);
    });

    it("ignores data that is not a job event", async () => {
        fake.on("GET", "/api/v1/jobs/{id}", () => json(200, job("Running")));
        start(`/jobs/${JOB_ID}`);
        await screen.findByText("Running", { selector: '[data-slot="badge"]' });
        const stream = FakeEventSource.latest() as unknown as {
            listeners: Map<string, ((event: MessageEvent<string>) => void)[]>;
        };

        act(() => {
            for (const listener of stream.listeners.get("transitioned") ?? []) {
                listener(
                    new MessageEvent("transitioned", { data: "not json" }),
                );
                listener(new MessageEvent("transitioned", { data: "{}" }));
            }
        });

        expect(events().queryAllByRole("listitem")).toHaveLength(0);
    });
});

describe("job commands", () => {
    it("reports what a cancellation achieved", async () => {
        fake.on("GET", "/api/v1/jobs/{id}", () =>
            json(200, job("ReadyToCommit", { publication_started: true })),
        ).on("POST", "/api/v1/jobs/{id}/cancel", () =>
            json(
                200,
                schema("CancelJobResult", {
                    job_id: JOB_ID,
                    state: "Committing",
                    revision: 4,
                    cancellation: "too_late_publication_started",
                    cancellation_requested: true,
                    publication_started: true,
                }),
            ),
        );
        const user = start(`/jobs/${JOB_ID}`);

        await user.click(
            await screen.findByRole("button", { name: "Cancel job" }),
        );
        await user.type(
            await screen.findByLabelText("Cancellation reason (optional)"),
            " wrong table ",
        );
        await user.click(
            screen.getByRole("button", { name: "Request cancellation" }),
        );

        expect(
            await screen.findByText(
                "Too late: a commit was already authorized and may still publish.",
            ),
        ).toBeInTheDocument();
        const request = fake.sent("POST", `/api/v1/jobs/${JOB_ID}/cancel`)[0];
        expect(request?.headers.get("x-csrf-token")).toBe("csrf-value");
        expect(await request?.json()).toEqual({ reason: "wrong table" });
    });

    it("gates cancellation and retry on the job table's grant coverage", async () => {
        const otherTable = "0190f3c4-0000-7000-8000-000000000002";
        fake.on("GET", "/auth/session", () => json(200, session("Viewer"))).on(
            "GET",
            "/api/v1/jobs/{id}",
            () => json(200, job("Running")),
        );
        start(`/jobs/${JOB_ID}`);
        expect(await screen.findByText("Requested by")).toBeInTheDocument();
        expect(
            screen.queryByRole("button", { name: "Cancel job" }),
        ).not.toBeInTheDocument();
        cleanup();

        // The grant covers another table, so neither command is offered.
        fake.on("GET", "/auth/session", () =>
            json(
                200,
                session("Operator", {
                    catalog_connection: null,
                    table: otherTable,
                }),
            ),
        ).on("GET", "/api/v1/jobs/{id}", () => json(200, job("Failed")));
        start(`/jobs/${JOB_ID}`);
        expect(await screen.findByText("Requested by")).toBeInTheDocument();
        expect(
            screen.queryByRole("button", { name: "Retry with a fresh plan" }),
        ).not.toBeInTheDocument();
        cleanup();

        // A connection grant for an alias covers the job's table.
        fake.on("GET", "/auth/session", () =>
            json(
                200,
                session("Operator", {
                    catalog_connection: CATALOG_ID,
                    table: null,
                }),
            ),
        ).on("GET", "/api/v1/jobs/{id}", () => json(200, job("Running")));
        start(`/jobs/${JOB_ID}`);
        expect(
            await screen.findByRole("button", { name: "Cancel job" }),
        ).toBeInTheDocument();
    });

    it("refreshes gated commands when the session's permissions change", async () => {
        let current = session("Operator");
        fake.on("GET", "/auth/session", () => json(200, current)).on(
            "GET",
            "/api/v1/jobs/{id}",
            () => json(200, job("Failed")),
        );
        start(`/jobs/${JOB_ID}`);
        expect(
            await screen.findByRole("button", {
                name: "Retry with a fresh plan",
            }),
        ).toBeInTheDocument();

        current = session("Viewer");
        act(() => {
            focusManager.setFocused(false);
            focusManager.setFocused(true);
        });
        await waitFor(() => {
            expect(
                screen.queryByRole("button", {
                    name: "Retry with a fresh plan",
                }),
            ).not.toBeInTheDocument();
        });

        current = session("Operator");
        act(() => {
            focusManager.setFocused(false);
            focusManager.setFocused(true);
        });
        expect(
            await screen.findByRole("button", {
                name: "Retry with a fresh plan",
            }),
        ).toBeInTheDocument();
        focusManager.setFocused(undefined);
    });

    it("sends no reason when none is given", async () => {
        fake.on("GET", "/api/v1/jobs/{id}", () => json(200, job("Queued"))).on(
            "POST",
            "/api/v1/jobs/{id}/cancel",
            () =>
                json(
                    200,
                    schema("CancelJobResult", {
                        job_id: JOB_ID,
                        state: "Cancelling",
                        revision: 2,
                        cancellation: "cancelling",
                        cancellation_requested: true,
                        publication_started: false,
                    }),
                ),
        );
        const user = start(`/jobs/${JOB_ID}`);

        await user.click(
            await screen.findByRole("button", { name: "Cancel job" }),
        );
        await user.click(
            await screen.findByRole("button", { name: "Request cancellation" }),
        );

        expect(
            await screen.findByText(
                "Cancellation accepted; the job is stopping.",
            ),
        ).toBeInTheDocument();
        expect(
            await fake.sent("POST", `/api/v1/jobs/${JOB_ID}/cancel`)[0]?.json(),
        ).toEqual({
            reason: null,
        });
    });

    it("keeps the reported outcome when the job ends meanwhile, until closed", async () => {
        let current = job("Queued");
        fake.on("GET", "/api/v1/jobs/{id}", () => json(200, current)).on(
            "POST",
            "/api/v1/jobs/{id}/cancel",
            () => {
                current = job("Cancelled", { cancellation_requested: true });
                return json(
                    200,
                    schema("CancelJobResult", {
                        job_id: JOB_ID,
                        state: "Cancelled",
                        revision: 2,
                        cancellation: "cancelled",
                        cancellation_requested: true,
                        publication_started: false,
                    }),
                );
            },
        );
        const user = start(`/jobs/${JOB_ID}`);

        await user.click(
            await screen.findByRole("button", { name: "Cancel job" }),
        );
        await user.click(
            await screen.findByRole("button", { name: "Request cancellation" }),
        );

        // The refreshed job is terminal, yet the dialog still reports.
        expect(
            await screen.findByText("Stopped: cancelled"),
        ).toBeInTheDocument();
        const dialog = screen.getByRole("dialog");
        expect(
            within(dialog).getByText("The job is already cancelled."),
        ).toBeInTheDocument();
        await user.click(within(dialog).getByRole("button", { name: "Done" }));

        await waitFor(() => {
            expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
        });
        expect(
            screen.queryByRole("button", { name: "Cancel job" }),
        ).not.toBeInTheDocument();
    });

    it("sends nothing when the confirmation is dismissed", async () => {
        fake.on("GET", "/api/v1/jobs/{id}", () => json(200, job("Running")));
        const user = start(`/jobs/${JOB_ID}`);

        await user.click(
            await screen.findByRole("button", { name: "Cancel job" }),
        );
        await user.click(
            await screen.findByRole("button", { name: "Keep running" }),
        );

        await waitFor(() => {
            expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
        });
        expect(fake.sent("POST", `/api/v1/jobs/${JOB_ID}/cancel`)).toEqual([]);
    });

    it("retries a failed job with a fresh plan and opens the new job", async () => {
        fake.on("GET", "/api/v1/jobs/{id}", (_request, params) =>
            params["id"] === RETRY_JOB_ID
                ? json(
                      200,
                      job("Planning", {
                          id: RETRY_JOB_ID,
                          retry_of_job_id: JOB_ID,
                      }),
                  )
                : json(200, job("Failed", { state_reason: "plan_rejected" })),
        ).on("POST", "/api/v1/jobs/{id}/retry", () =>
            json(
                202,
                schema("AcceptedRetry", {
                    operation_id: RETRY_JOB_ID,
                    job_id: RETRY_JOB_ID,
                    resource_url: `/api/v1/jobs/${RETRY_JOB_ID}`,
                    retry_of_job_id: JOB_ID,
                }),
            ),
        );
        const user = start(`/jobs/${JOB_ID}`);

        expect(await screen.findByText(/Plan rejected/)).toBeInTheDocument();
        await user.click(
            screen.getByRole("button", { name: "Retry with a fresh plan" }),
        );

        expect(await screen.findByText(RETRY_JOB_ID)).toBeInTheDocument();
        expect(window.location.pathname).toBe(`/jobs/${RETRY_JOB_ID}`);
        expect(
            screen.getByRole("link", { name: `Job ${JOB_ID.slice(-8)}` }),
        ).toHaveAttribute("href", `/jobs/${JOB_ID}`);
        const request = fake.sent("POST", `/api/v1/jobs/${JOB_ID}/retry`)[0];
        expect(request?.headers.get("Idempotency-Key")).toMatch(
            /^[0-9a-f-]{36}$/,
        );
        // The new job's page streams its own events.
        expect(FakeEventSource.latest().url).toBe(
            `/api/v1/jobs/${RETRY_JOB_ID}/events`,
        );
    });

    it("shows a typed refusal to retry", async () => {
        fake.on("GET", "/api/v1/jobs/{id}", () =>
            json(200, job("Cancelled")),
        ).on("POST", "/api/v1/jobs/{id}/retry", () =>
            apiError(
                409,
                "job_already_succeeded",
                "PermanentFailure",
                "open_job",
            ),
        );
        const user = start(`/jobs/${JOB_ID}`);

        await user.click(
            await screen.findByRole("button", {
                name: "Retry with a fresh plan",
            }),
        );

        expect(
            await screen.findByText("Job already succeeded"),
        ).toBeInTheDocument();
    });

    it("marks ReplanRequired distinctly and offers a fresh plan", async () => {
        fake.on("GET", "/api/v1/jobs/{id}", () =>
            json(200, job("ReplanRequired")),
        );
        start(`/jobs/${JOB_ID}`);

        expect(await screen.findByText("Replan required")).toBeInTheDocument();
        expect(
            screen.getByText("Stopped: replan required"),
        ).toBeInTheDocument();
        expect(
            screen.getByRole("button", { name: "Retry with a fresh plan" }),
        ).toBeInTheDocument();
    });

    it("marks CommitOutcomeUnknown distinctly and does not offer a retry", async () => {
        fake.on("GET", "/api/v1/jobs/{id}", () =>
            json(
                200,
                job("CommitOutcomeUnknown", { publication_started: true }),
            ),
        );
        start(`/jobs/${JOB_ID}`);

        expect(
            await screen.findByText("Commit outcome unknown"),
        ).toBeInTheDocument();
        expect(screen.getByText("Outcome unknown")).toBeInTheDocument();
        expect(
            screen.queryByRole("button", { name: "Retry with a fresh plan" }),
        ).not.toBeInTheDocument();
    });
});

describe("jobs and audit", () => {
    it("lists jobs with execution and publication side by side", async () => {
        fake.on("GET", "/api/v1/jobs", () =>
            json(
                200,
                schema("JobPage", {
                    items: [
                        job("ReadyToCommit"),
                        job("NoOp", { id: RETRY_JOB_ID, table_aliases: [] }),
                    ],
                    next_cursor: null,
                }),
            ),
        );
        start("/jobs");

        const rows = await screen.findAllByRole("row");
        // The table's first visible name, or its short ID without one.
        expect(
            await within(rows[1] as HTMLElement).findByRole("link", {
                name: "lake.sales.orders",
            }),
        ).toHaveAttribute("title", TABLE_ID);
        expect(
            within(rows[2] as HTMLElement).getByRole("link", {
                name: `Table ${TABLE_ID.slice(-8)}`,
            }),
        ).toHaveAttribute("href", `/tables/${TABLE_ID}`);
        // The name leads; the job ID is a secondary link and a copy affordance.
        expect(
            within(rows[1] as HTMLElement).getByRole("link", {
                name: `Job ${JOB_ID.slice(-8)}`,
            }),
        ).toHaveAttribute("href", `/jobs/${JOB_ID}`);
        expect(
            within(rows[1] as HTMLElement).getByRole("button", {
                name: "Copy job ID",
            }),
        ).toBeInTheDocument();
        expect(
            within(rows[1] as HTMLElement).getByText("Alice"),
        ).toBeInTheDocument();
        expect(
            screen.queryByRole("columnheader", { name: "Job" }),
        ).not.toBeInTheDocument();
        expect(screen.queryByText(JOB_ID)).not.toBeInTheDocument();
        expect(
            within(rows[1] as HTMLElement).getByText("Outputs ready"),
        ).toBeInTheDocument();
        expect(
            within(rows[1] as HTMLElement).getByText("Not started"),
        ).toBeInTheDocument();
        expect(
            within(rows[2] as HTMLElement).getByText("Nothing to rewrite"),
        ).toBeInTheDocument();
        expect(
            within(rows[2] as HTMLElement).getByText("Not needed"),
        ).toBeInTheDocument();
    });

    it("hides audit history without an organization-wide administrator grant", async () => {
        fake.on("GET", "/api/v1/audit", () =>
            apiError(
                403,
                "permission_denied",
                "PermanentFailure",
                "request_permission",
            ),
        );
        fake.on("GET", "/api/v1/jobs", () =>
            json(200, { items: [], next_cursor: null }),
        );
        start("/jobs");
        expect(
            await screen.findByText("No jobs are visible to you."),
        ).toBeInTheDocument();
        expect(
            screen.queryByRole("link", { name: "Audit" }),
        ).not.toBeInTheDocument();
        cleanupAndStart("/audit");

        expect(
            await screen.findByText(
                /Audit history requires an organization-wide grant/,
            ),
        ).toBeInTheDocument();
    });

    it("shows audit history to an administrator", async () => {
        fake.on("GET", "/auth/session", () =>
            json(200, session("Administrator")),
        ).on("GET", "/api/v1/audit", () =>
            json(
                200,
                schema("AuditEventPage", {
                    items: [
                        {
                            id: "9007199254740993",
                            actor_principal_id: null,
                            actor_display_name: null,
                            actor_kind: "Anonymous",
                            action: "session.denied",
                            resource_type: "session",
                            resource_id: null,
                            outcome: "Denied",
                            detail: { reason: "invalid_csrf" },
                            occurred_at: "2026-10-21T10:00:00Z",
                        },
                        {
                            id: "9007199254740994",
                            actor_principal_id: PRINCIPAL_ID,
                            actor_display_name: "Alice",
                            actor_kind: "User",
                            action: "job.submitted",
                            resource_type: "job",
                            resource_id: JOB_ID,
                            outcome: "Allowed",
                            detail: {},
                            occurred_at: "2026-10-21T10:01:00Z",
                        },
                    ],
                    next_cursor: null,
                }),
            ),
        );
        start("/audit");

        expect(await screen.findByText("session.denied")).toBeInTheDocument();
        // A user is named, with the principal ID kept in the title; an
        // anonymous actor has no principal, so only its kind shows.
        const grid = within(screen.getByRole("table"));
        expect(grid.getByText("Alice")).toHaveAttribute("title", PRINCIPAL_ID);
        expect(
            grid.getByRole("button", { name: "Copy principal ID" }),
        ).toBeInTheDocument();
        expect(grid.getByText("Anonymous")).toBeInTheDocument();
        expect(grid.queryByText(PRINCIPAL_ID)).not.toBeInTheDocument();
        // A resource with no name shows its type, then a short ID and copy
        // affordance; known resources link to their detail page.
        expect(
            grid.getByRole("link", { name: `Job ${JOB_ID.slice(-8)}` }),
        ).toHaveAttribute("href", `/jobs/${JOB_ID}`);
        expect(
            grid.getByRole("button", { name: "Copy job ID" }),
        ).toBeInTheDocument();
        expect(
            grid.getByText(new RegExp(JOB_ID), { selector: ".sr-only" }),
        ).toBeInTheDocument();
        expect(grid.queryByText(JOB_ID)).not.toBeInTheDocument();
        expect(screen.getByRole("link", { name: "Audit" })).toHaveAttribute(
            "aria-current",
            "page",
        );
        expect(screen.getByRole("link", { name: "Jobs" })).not.toHaveAttribute(
            "aria-current",
        );
        expect(
            screen.getByText('{"reason":"invalid_csrf"}'),
        ).toBeInTheDocument();
    });
});

describe("workers", () => {
    it("shows worker health to an administrator", async () => {
        fake.on("GET", "/auth/session", () =>
            json(200, session("Administrator")),
        ).on("GET", "/api/v1/workers", () =>
            json(
                200,
                schema("WorkerSessionPage", {
                    items: [
                        {
                            id: "0190f3c4-0000-7000-8000-0000000000f1",
                            principal_id: PRINCIPAL_ID,
                            pool: "native",
                            worker_name: "worker-1",
                            build_version: "0.1.0",
                            capabilities: ["native-v1"],
                            memory_bytes: 2 * 1024 ** 3,
                            cpu_millis: 1500,
                            status: "draining",
                            registered_at: "2026-10-21T10:00:00Z",
                            lease_expires_at: "2026-10-21T10:01:30Z",
                            running_attempt_id:
                                "0190f3c4-0000-7000-8000-0000000000f2",
                        },
                        {
                            id: "0190f3c4-0000-7000-8000-0000000000f3",
                            principal_id: PRINCIPAL_ID,
                            pool: "native",
                            worker_name: "worker-2",
                            build_version: "0.1.0",
                            capabilities: ["native-v1"],
                            memory_bytes: 1024 ** 3,
                            cpu_millis: 1000,
                            status: "expired",
                            registered_at: "2026-10-21T09:00:00Z",
                            lease_expires_at: "2026-10-21T09:01:30Z",
                            running_attempt_id: null,
                        },
                    ],
                    next_cursor: null,
                }),
            ),
        );
        start("/workers");

        const rows = await screen.findAllByRole("row");
        expect(
            screen.getByRole("link", { name: "Workers" }),
        ).toBeInTheDocument();
        const first = within(rows[1] as HTMLElement);
        expect(first.getByText("worker-1")).toBeInTheDocument();
        expect(first.getByText("Draining")).toBeInTheDocument();
        expect(first.getByText("000000f2")).toBeInTheDocument();
        expect(first.getByText("2.0 GiB, 1.5 CPU")).toBeInTheDocument();
        const second = within(rows[2] as HTMLElement);
        expect(second.getByText("Expired")).toBeInTheDocument();
        expect(second.getByText("Idle")).toBeInTheDocument();
    });

    it("explains the grant worker health requires", async () => {
        fake.on("GET", "/api/v1/workers", () =>
            apiError(
                403,
                "permission_denied",
                "PermanentFailure",
                "request_permission",
            ),
        );
        start("/workers");

        expect(
            await screen.findByText(
                /Worker health requires an organization-wide grant/,
            ),
        ).toBeInTheDocument();
        expect(
            screen.queryByRole("link", { name: "Workers" }),
        ).not.toBeInTheDocument();
    });

    it("says when no worker has registered", async () => {
        fake.on("GET", "/auth/session", () =>
            json(200, session("Administrator")),
        ).on("GET", "/api/v1/workers", () =>
            json(
                200,
                schema("WorkerSessionPage", { items: [], next_cursor: null }),
            ),
        );
        start("/workers");

        expect(
            await screen.findByText("No worker has registered yet."),
        ).toBeInTheDocument();
    });
});

function cleanupAndStart(path: string) {
    return start(path);
}
