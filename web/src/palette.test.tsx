// The command palette and live list updates (ADR 0067), through the whole
// app against the contract-typed fake coordinator.

import { type QueryClient, QueryClientProvider } from "@tanstack/react-query";
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
import { isPaletteShortcut } from "./components/CommandPaletteButton";
import { createQueryClient } from "./queryClient";
import { FakeApi, FakeEventSource, json, schema } from "./test/fakes";
import {
    CATALOG_ID,
    JOB_ID,
    PRINCIPAL_ID,
    TABLE_ID,
    catalogs,
    job,
    overview,
    session,
} from "./test/fixtures";

let fake: FakeApi;
let client: QueryClient;

const results = schema("SearchResults", {
    catalogs: [{ id: CATALOG_ID, name: "lake", enabled: false }],
    tables: [
        {
            id: TABLE_ID,
            catalog_id: CATALOG_ID,
            namespace: ["sales"],
            name: "orders",
        },
    ],
    principals: null,
});

beforeEach(() => {
    fake = new FakeApi();
    vi.stubGlobal("fetch", fake.fetch);
    document.cookie = "bergsmith_csrf=csrf-value";
    fake.on("GET", "/auth/session", () => json(200, session()))
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs))
        .on("GET", "/api/v1/overview", () => json(200, overview))
        .on("GET", "/api/v1/jobs", () =>
            json(
                200,
                schema("JobPage", {
                    items: [job("Running")],
                    next_cursor: null,
                }),
            ),
        )
        .on("GET", "/api/v1/search", () => json(200, results));
});

afterEach(() => {
    expect(fake.unrouted).toEqual([]);
});

function start(path: string) {
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

function writes() {
    return fake.requests.filter((request) => request.method !== "GET");
}

async function openPalette(user: ReturnType<typeof userEvent.setup>) {
    await screen.findByRole("heading", { name: "Overview" });
    await user.keyboard("{Control>}k{/Control}");
    return screen.findByRole("combobox", {
        name: "Search pages, catalogs, tables, and people",
    });
}

it("opens with the shortcut and goes to a page from the keyboard", async () => {
    const user = start("/");
    const field = await openPalette(user);
    const results = screen.getByRole("listbox", { name: "Results" });

    expect(
        within(results).getByRole("option", { name: "Overview" }),
    ).toHaveAttribute("aria-selected", "true");
    expect(
        within(results).queryByRole("option", { name: /Audit/ }),
    ).not.toBeInTheDocument();

    await user.type(field, "jo");
    expect(within(results).getAllByRole("option")).toHaveLength(1);
    const jobs = within(results).getByRole("option", { name: "Jobs" });
    expect(field).toHaveAttribute("aria-activedescendant", jobs.id);
    await user.keyboard("{Enter}");

    expect(
        await screen.findByRole("heading", { name: "Jobs" }),
    ).toBeInTheDocument();
    expect(window.location.pathname).toBe("/jobs");
    expect(screen.queryByRole("combobox")).not.toBeInTheDocument();
    expect(writes()).toEqual([]);
});

it("finds catalogs and tables by name and links to them", async () => {
    const user = start("/");
    const field = await openPalette(user);
    await user.type(field, "lak");

    const results = screen.getByRole("listbox", { name: "Results" });
    const table = await within(results).findByRole("option", {
        name: /sales\.orders/,
    });
    expect(table).toHaveAttribute("href", `/tables/${TABLE_ID}`);
    await waitFor(() => {
        expect(table).toHaveTextContent("lake");
    });
    expect(
        within(results).getByRole("option", { name: /lake.*disabled/ }),
    ).toHaveAttribute("href", `/catalogs/${CATALOG_ID}`);
    expect(screen.getByRole("status")).toHaveTextContent("2 results");

    const searches = fake.sent("GET", "/api/v1/search");
    expect(searches).toHaveLength(1);
    expect(new URL(searches[0]?.url ?? "").searchParams.get("q")).toBe("lak");

    await user.keyboard("{ArrowDown}");
    expect(field).toHaveAttribute(
        "aria-activedescendant",
        within(results)
            .getByRole("option", { name: /sales\.orders/ })
            .getAttribute("id"),
    );
    await user.keyboard("{ArrowUp}{ArrowUp}");
    expect(field).toHaveAttribute(
        "aria-activedescendant",
        within(results)
            .getByRole("option", { name: /sales\.orders/ })
            .getAttribute("id"),
    );
    expect(writes()).toEqual([]);
});

it("waits for the newer search before Enter opens a found result", async () => {
    const user = start("/");
    const field = await openPalette(user);
    await user.type(field, "lak");
    const results = screen.getByRole("listbox", { name: "Results" });
    const lake = await within(results).findByRole("option", { name: /^lake/ });
    expect(field).toHaveAttribute("aria-activedescendant", lake.id);

    // The shown results answer "lak", not "lake".
    await user.type(field, "e{Enter}");
    expect(window.location.pathname).toBe("/");
    await waitFor(() => {
        expect(results).toHaveAttribute("aria-busy", "false");
    });
    await user.keyboard("{Enter}");
    await waitFor(() => {
        expect(window.location.pathname).toBe(`/catalogs/${CATALOG_ID}`);
    });
});

it("starts empty after the shortcut closes it", async () => {
    const user = start("/");
    const field = await openPalette(user);
    await user.type(field, "jo");
    await user.keyboard("{Control>}k{/Control}");
    await waitFor(() => {
        expect(screen.queryByRole("combobox")).not.toBeInTheDocument();
    });
    await user.keyboard("{Control>}k{/Control}");
    expect(
        await screen.findByRole("combobox", {
            name: "Search pages, catalogs, tables, and people",
        }),
    ).toHaveValue("");
});

it("leaves Ctrl+K to text fields where the shortcut is ⌘K", () => {
    const key = (init: KeyboardEventInit) => new KeyboardEvent("keydown", init);
    expect(isPaletteShortcut(key({ key: "k", metaKey: true }), true)).toBe(
        true,
    );
    expect(isPaletteShortcut(key({ key: "k", ctrlKey: true }), true)).toBe(
        false,
    );
    expect(isPaletteShortcut(key({ key: "K", ctrlKey: true }), false)).toBe(
        true,
    );
    expect(isPaletteShortcut(key({ key: "k", metaKey: true }), false)).toBe(
        false,
    );
    expect(
        isPaletteShortcut(
            key({ key: "k", ctrlKey: true, shiftKey: true }),
            false,
        ),
    ).toBe(false);
});

it("offers people only to those who manage access", async () => {
    fake.on("GET", "/auth/session", () =>
        json(200, session("Administrator")),
    ).on("GET", "/api/v1/search", () =>
        json(
            200,
            schema("SearchResults", {
                catalogs: [],
                tables: [],
                principals: [
                    {
                        id: PRINCIPAL_ID,
                        kind: "User",
                        display_name: "Alice",
                        active: false,
                    },
                ],
            }),
        ),
    );
    const user = start("/");
    const field = await openPalette(user);
    await user.type(field, "ali");

    expect(
        await screen.findByRole("option", { name: /Alice.*inactive/ }),
    ).toHaveAttribute("href", `/users/${PRINCIPAL_ID}`);
    expect(screen.getByRole("group", { name: "People" })).toBeInTheDocument();
});

it("says when nothing matches and shows a failed search", async () => {
    fake.on(
        "GET",
        "/api/v1/search",
        () =>
            json(
                200,
                schema("SearchResults", {
                    catalogs: [],
                    tables: [],
                    principals: null,
                }),
            ),
        () =>
            json(400, {
                reason: "invalid_request",
                disposition: "PermanentFailure",
                operator_action: "correct_the_request",
            }),
    );
    const user = start("/");
    const field = await openPalette(user);

    await user.type(field, "zzz");
    await waitFor(() => {
        expect(screen.getByRole("status")).toHaveTextContent(
            "No matches for “zzz”",
        );
    });

    await user.type(field, "z");
    expect(await screen.findByRole("alert")).toHaveTextContent(
        "Invalid request",
    );
});

it("opens from the top bar and returns focus when closed", async () => {
    const user = start("/");
    await screen.findByRole("heading", { name: "Overview" });
    const button = screen.getByRole("button", { name: /^Search/ });
    await user.click(button);

    const field = await screen.findByRole("combobox");
    await waitFor(() => {
        expect(field).toHaveFocus();
    });
    await user.keyboard("{Escape}");

    await waitFor(() => {
        expect(screen.queryByRole("combobox")).not.toBeInTheDocument();
    });
    expect(button).toHaveFocus();
    expect(fake.sent("GET", "/api/v1/search")).toEqual([]);
});

it("refreshes the jobs list when a shown job changes", async () => {
    start("/jobs");
    await screen.findByRole("link", { name: /orders/ });
    const stream = FakeEventSource.live();
    expect(stream.url).toBe("/api/v1/events");
    const before = fake.sent("GET", "/api/v1/jobs").length;

    fake.on("GET", "/api/v1/jobs", () =>
        json(
            200,
            schema("JobPage", {
                items: [job("Succeeded", { revision: 4 })],
                next_cursor: null,
            }),
        ),
    );
    act(() => {
        stream.send("ready", {}, "10");
    });
    await waitFor(() => {
        expect(fake.sent("GET", "/api/v1/jobs").length).toBe(before + 1);
    });
    act(() => {
        stream.send(
            "job",
            {
                job_id: JOB_ID,
                table_id: TABLE_ID,
                state: "Succeeded",
                revision: 4,
            },
            "11",
        );
    });
    // The list already shows that revision; only the counts refresh.
    await waitFor(() => {
        expect(fake.sent("GET", "/api/v1/jobs/facets").length).toBeGreaterThan(
            1,
        );
    });
    expect(fake.sent("GET", "/api/v1/jobs").length).toBe(before + 1);
});
