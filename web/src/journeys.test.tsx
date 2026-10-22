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
} from "./test/fixtures";

let fake: FakeApi;
let client: QueryClient;

beforeEach(() => {
    fake = new FakeApi();
    vi.stubGlobal("fetch", fake.fetch);
    document.cookie = "bergsmith_csrf=csrf-value";
    fake.on("GET", "/auth/session", () => json(200, session())).on(
        "GET",
        "/api/v1/catalogs",
        () => json(200, catalogs),
    );
});

afterEach(() => {
    expect(fake.unrouted).toEqual([]);
});

function start(path: string) {
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
        const user = start("/");

        expect(
            await screen.findByRole("link", { name: "lake.sales.orders" }),
        ).toHaveAttribute("href", `/tables/${TABLE_ID}`);
        // The home page is the tables page, under one address.
        expect(window.location.pathname).toBe("/tables");
        expect(screen.getByRole("link", { name: "Tables" })).toHaveAttribute(
            "aria-current",
            "page",
        );
        expect(screen.getByText("lake.legacy.orders_v1")).toBeInTheDocument();
        await user.click(
            screen.getByRole("button", { name: "Load more tables" }),
        );

        expect(
            await screen.findByRole("link", {
                name: "0190f3c4-0000-7000-8000-000000000002",
            }),
        ).toBeInTheDocument();
        expect(
            new URL(
                fake.sent("GET", "/api/v1/tables")[1]?.url ?? "",
            ).searchParams.get("cursor"),
        ).toBe("next-page");
        expect(
            screen.queryByRole("button", { name: "Load more tables" }),
        ).not.toBeInTheDocument();
    });

    it("opens a table from the list", async () => {
        fake.on("GET", "/api/v1/tables", () =>
            json(200, { items: [table], next_cursor: null }),
        ).on("GET", "/api/v1/tables/{id}", () => json(200, table));
        const user = start("/tables");

        await user.click(
            await screen.findByRole("link", { name: "lake.sales.orders" }),
        );

        expect(
            await screen.findByRole("heading", { name: "lake.sales.orders" }),
        ).toBeInTheDocument();
        expect(window.location.pathname).toBe(`/tables/${TABLE_ID}`);
    });

    it("names a catalog by ID when its name is unknown", async () => {
        fake.on("GET", "/api/v1/catalogs", () =>
            json(200, { items: [], next_cursor: null }),
        ).on("GET", "/api/v1/tables/{id}", () => json(200, table));
        start(`/tables/${TABLE_ID}`);

        expect(
            await screen.findByRole("heading", {
                name: `${CATALOG_ID}.sales.orders`,
            }),
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
            screen.queryByRole("button", { name: "Request cancellation" }),
        ).not.toBeInTheDocument();
        expect(
            screen.queryByRole("button", { name: "Retry with a fresh plan" }),
        ).not.toBeInTheDocument();
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
        expect(screen.getByRole("link", { name: JOB_ID })).toHaveAttribute(
            "href",
            `/jobs/${JOB_ID}`,
        );
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
                        job("NoOp", { id: RETRY_JOB_ID }),
                    ],
                    next_cursor: null,
                }),
            ),
        );
        start("/jobs");

        const rows = await screen.findAllByRole("row");
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
                            actor_kind: "Anonymous",
                            action: "session.denied",
                            resource_type: "session",
                            resource_id: null,
                            outcome: "Denied",
                            detail: { reason: "invalid_csrf" },
                            occurred_at: "2026-10-21T10:00:00Z",
                        },
                    ],
                    next_cursor: null,
                }),
            ),
        );
        start("/audit");

        expect(await screen.findByText("session.denied")).toBeInTheDocument();
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
        expect(first.getByText("0190f3c4")).toBeInTheDocument();
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
    cleanup();
    return start(path);
}
