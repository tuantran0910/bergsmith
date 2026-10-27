import { QueryClientProvider } from "@tanstack/react-query";
import {
    act,
    configure,
    render,
    screen,
    waitFor,
    within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { App } from "./App";
import type { Schemas } from "./api/client";
import { createQueryClient } from "./queryClient";
import { apiError, FakeApi, json, schema } from "./test/fakes";
import { PRINCIPAL_ID, TABLE_ID, session } from "./test/fixtures";

// The first route load also transforms the lazy page chunk.
configure({ asyncUtilTimeout: 5_000 });

const DESTINATION_ID = "0190f3c4-0000-7000-8000-0000000000c7";
const CONNECTION_ID = "0190f3c4-0000-7000-8000-0000000000c8";
const RULE_ID = "0190f3c4-0000-7000-8000-0000000000c9";

function destination(
    overrides: Partial<Schemas["NotificationDestination"]> = {},
): Schemas["NotificationDestination"] {
    return schema("NotificationDestination", {
        id: DESTINATION_ID,
        name: "ops-webhook",
        kind: "webhook",
        signing_secret_ref: "hooks/signing",
        enabled: true,
        revision: 2,
        created_by: PRINCIPAL_ID,
        created_at: "2026-10-01T00:00:00Z",
        updated_at: "2026-10-20T00:00:00Z",
        url_ref: "hooks/ops-url",
        ...overrides,
    });
}

function destinationPage(
    items: Schemas["NotificationDestination"][],
): Schemas["NotificationDestinationPage"] {
    return schema("NotificationDestinationPage", {
        items,
        next_cursor: null,
    });
}

function rule(
    overrides: Partial<Schemas["NotificationRule"]> = {},
): Schemas["NotificationRule"] {
    return schema("NotificationRule", {
        id: RULE_ID,
        name: "failures",
        destination_id: DESTINATION_ID,
        event_kind: "job.replan_required",
        filter: { connection_id: CONNECTION_ID },
        repeat_threshold: 3,
        enabled: true,
        revision: 2,
        created_by: PRINCIPAL_ID,
        created_at: "2026-10-01T00:00:00Z",
        updated_at: "2026-10-20T00:00:00Z",
        ...overrides,
    });
}

function rulePage(
    items: Schemas["NotificationRule"][],
): Schemas["NotificationRulePage"] {
    return schema("NotificationRulePage", { items, next_cursor: null });
}

let fake: FakeApi;
beforeEach(() => {
    fake = new FakeApi();
    vi.stubGlobal("fetch", fake.fetch);
    document.cookie = "bergsmith_csrf=csrf-value";
    fake.on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/notifications/destinations", () =>
            json(200, destinationPage([destination()])),
        )
        .on("GET", "/api/v1/notifications/rules", () =>
            json(200, rulePage([rule()])),
        );
});
afterEach(() => {
    expect(fake.unrouted).toEqual([]);
});

let queryClient = createQueryClient(0);

function start(path: string) {
    window.history.replaceState(null, "", path);
    queryClient = createQueryClient(0);
    render(
        <QueryClientProvider client={queryClient}>
            <App />
        </QueryClientProvider>,
    );
    return userEvent.setup();
}

describe("notification destinations", () => {
    it("lists destinations with their kind, reference, and state", async () => {
        start("/notifications");
        const table = await screen.findByRole("table", {
            name: "Notification destinations",
        });
        expect(
            screen.getByRole("heading", { name: "Notifications" }),
        ).toBeVisible();
        expect(within(table).getByText("ops-webhook")).toBeVisible();
        expect(within(table).getByText("Webhook")).toBeVisible();
        expect(within(table).getByText("hooks/ops-url")).toBeVisible();
        expect(within(table).getByText("Enabled")).toBeVisible();
        expect(within(table).getByText("2")).toBeVisible();
    });

    it("does not offer destinations to a session without organization configuration", async () => {
        fake.on("GET", "/auth/session", () => json(200, session("Viewer")));
        start("/notifications");
        expect(await screen.findByText("Permission required")).toBeVisible();
        expect(screen.getByText(/organization configuration/)).toBeVisible();
        expect(
            screen.queryByRole("table", {
                name: "Notification destinations",
            }),
        ).not.toBeInTheDocument();
        expect(
            fake.sent("GET", "/api/v1/notifications/destinations"),
        ).toHaveLength(0);
    });

    it("creates a destination once with the typed fields and refreshes the list", async () => {
        const created = destination({
            name: "nightly-hooks",
            revision: 1,
            url_ref: "hooks/nightly-url",
            signing_secret_ref: "hooks/nightly-signing",
        });
        fake.on("POST", "/api/v1/notifications/destinations", () =>
            json(201, created),
        );
        const user = start("/notifications");
        await user.click(
            await screen.findByRole("button", { name: "New destination" }),
        );
        await user.type(
            screen.getByLabelText("Destination name"),
            "nightly-hooks",
        );
        await user.type(
            screen.getByLabelText("URL reference"),
            "hooks/nightly-url",
        );
        await user.type(
            screen.getByLabelText("Signing secret reference"),
            "hooks/nightly-signing",
        );
        await user.click(
            screen.getByRole("button", { name: "Create destination" }),
        );
        await waitFor(() => {
            expect(
                screen.queryByRole("heading", { name: "New destination" }),
            ).not.toBeInTheDocument();
        });
        expect(
            fake.sent("POST", "/api/v1/notifications/destinations"),
        ).toHaveLength(1);
        const request = fake.sent(
            "POST",
            "/api/v1/notifications/destinations",
        )[0];
        expect(request?.headers.get("x-csrf-token")).toBe("csrf-value");
        expect(await request?.json()).toEqual({
            name: "nightly-hooks",
            kind: "webhook",
            url_ref: "hooks/nightly-url",
            signing_secret_ref: "hooks/nightly-signing",
            enabled: true,
        });
        expect(screen.getByText("Created nightly-hooks.")).toBeVisible();
        await waitFor(() => {
            expect(
                fake.sent("GET", "/api/v1/notifications/destinations"),
            ).toHaveLength(2);
        });
    });

    it("keeps a stale edit open and shows the revision conflict", async () => {
        fake.on(
            "GET",
            "/api/v1/notifications/destinations",
            () => json(200, destinationPage([destination()])),
            () => json(200, destinationPage([destination({ revision: 3 })])),
        ).on("PATCH", "/api/v1/notifications/destinations/{id}", () =>
            apiError(
                409,
                "revision_conflict",
                "PermanentFailure",
                "reload_and_retry",
            ),
        );
        const user = start("/notifications");
        await user.click(
            await screen.findByRole("button", { name: "Edit ops-webhook" }),
        );
        const name = screen.getByLabelText("Destination name");
        await user.clear(name);
        await user.type(name, "ops-webhook-2");
        await user.click(
            screen.getByRole("button", { name: "Save destination" }),
        );
        expect(await screen.findByText("Revision conflict")).toBeVisible();
        expect(
            screen.getByRole("heading", { name: "Edit ops-webhook" }),
        ).toBeVisible();
        expect(screen.getByLabelText("Destination name")).toHaveValue(
            "ops-webhook-2",
        );
        expect(
            await screen.findByRole("button", { name: "Reload revision 3" }),
        ).toBeVisible();
        expect(
            screen.getByRole("button", { name: "Save destination" }),
        ).toBeDisabled();
        const request = fake.sent(
            "PATCH",
            `/api/v1/notifications/destinations/${DESTINATION_ID}`,
        )[0];
        expect(await request?.json()).toEqual({
            revision: 2,
            name: "ops-webhook-2",
        });
    });

    it("keeps the opened revision when the list refreshes before save", async () => {
        fake.on(
            "GET",
            "/api/v1/notifications/destinations",
            () => json(200, destinationPage([destination()])),
            () => json(200, destinationPage([destination({ revision: 3 })])),
        ).on("PATCH", "/api/v1/notifications/destinations/{id}", () =>
            apiError(
                409,
                "revision_conflict",
                "PermanentFailure",
                "reload_and_retry",
            ),
        );
        const user = start("/notifications");
        await user.click(
            await screen.findByRole("button", { name: "Edit ops-webhook" }),
        );
        const name = screen.getByLabelText("Destination name");
        await user.clear(name);
        await user.type(name, "ops-webhook-2");
        // Another administrator saves while this edit is open; the list
        // refreshes without adopting the newer revision into the draft.
        await act(async () => {
            await queryClient.invalidateQueries({
                queryKey: ["notifications", "destinations"],
            });
        });
        await waitFor(() => {
            expect(
                fake.sent("GET", "/api/v1/notifications/destinations"),
            ).toHaveLength(2);
        });
        await user.click(
            screen.getByRole("button", { name: "Save destination" }),
        );
        await waitFor(() => {
            expect(
                fake.sent(
                    "PATCH",
                    `/api/v1/notifications/destinations/${DESTINATION_ID}`,
                ),
            ).toHaveLength(1);
        });
        const request = fake.sent(
            "PATCH",
            `/api/v1/notifications/destinations/${DESTINATION_ID}`,
        )[0];
        expect(await request?.json()).toEqual({
            revision: 2,
            name: "ops-webhook-2",
        });
    });

    it("loads later destination pages into the rule selector", async () => {
        const second = destination({
            id: "0190f3c4-0000-7000-8000-0000000000ca",
            name: "second-hook",
        });
        fake.on(
            "GET",
            "/api/v1/notifications/destinations",
            () =>
                json(
                    200,
                    schema("NotificationDestinationPage", {
                        items: [destination()],
                        next_cursor: "next",
                    }),
                ),
            () => json(200, destinationPage([second])),
        );
        const user = start("/notifications/rules");
        await user.click(
            await screen.findByRole("button", { name: "New rule" }),
        );
        await user.click(
            await screen.findByRole("button", {
                name: "Load more destinations",
            }),
        );
        await user.click(screen.getByRole("combobox", { name: "Destination" }));
        expect(
            await screen.findByRole("option", { name: "second-hook" }),
        ).toBeVisible();
    });

    it("reports a delivered test without showing the response body", async () => {
        fake.on("POST", "/api/v1/notifications/destinations/{id}/test", () =>
            json(
                200,
                schema("NotificationDeliveryTest", {
                    delivered: true,
                    status: 204,
                    response: "internal response body",
                }),
            ),
        );
        const user = start("/notifications");
        await user.click(
            await screen.findByRole("button", { name: "Test ops-webhook" }),
        );
        expect(await screen.findByText("Test delivered (204).")).toBeVisible();
        expect(screen.queryByText("internal response body")).toBeNull();
        const request = fake.sent(
            "POST",
            `/api/v1/notifications/destinations/${DESTINATION_ID}/test`,
        )[0];
        expect(request?.headers.get("x-csrf-token")).toBe("csrf-value");
    });

    it("reports a refused test without showing the response body", async () => {
        fake.on("POST", "/api/v1/notifications/destinations/{id}/test", () =>
            json(
                200,
                schema("NotificationDeliveryTest", {
                    delivered: false,
                    status: 404,
                    response: "another response body",
                }),
            ),
        );
        const user = start("/notifications");
        await user.click(
            await screen.findByRole("button", { name: "Test ops-webhook" }),
        );
        expect(
            await screen.findByText("The destination answered 404."),
        ).toBeVisible();
        expect(screen.queryByText("another response body")).toBeNull();
    });
});

describe("notification rules", () => {
    it("lists rules with the destination name and filter summary", async () => {
        start("/notifications/rules");
        const table = await screen.findByRole("table", {
            name: "Notification rules",
        });
        expect(within(table).getByText("failures")).toBeVisible();
        expect(within(table).getByText("Job replan required")).toBeVisible();
        expect(within(table).getByText("ops-webhook")).toBeVisible();
        expect(
            within(table).getByText(`Connection ${CONNECTION_ID.slice(-8)}`),
        ).toBeVisible();
        expect(within(table).getByText("3")).toBeVisible();
        expect(within(table).getByText("Enabled")).toBeVisible();
    });

    it("creates a rule with the chosen filter and threshold", async () => {
        const created = rule({
            name: "missed-1",
            filter: { table_id: TABLE_ID },
            repeat_threshold: 3,
            revision: 1,
        });
        fake.on("POST", "/api/v1/notifications/rules", () =>
            json(201, created),
        );
        const user = start("/notifications/rules");
        await user.click(
            await screen.findByRole("button", { name: "New rule" }),
        );
        await user.type(screen.getByLabelText("Rule name"), "missed-1");
        await user.click(screen.getByRole("combobox", { name: "Event" }));
        await user.click(
            screen.getByRole("option", { name: "Job replan required" }),
        );
        await waitFor(() => {
            expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
        });
        await user.click(screen.getByRole("combobox", { name: "Filter" }));
        await user.click(screen.getByRole("option", { name: "Table" }));
        await waitFor(() => {
            expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
        });
        await user.type(screen.getByLabelText("Filter value"), TABLE_ID);
        await user.type(await screen.findByLabelText("Repeat threshold"), "3");
        await user.click(screen.getByRole("button", { name: "Create rule" }));
        await waitFor(() => {
            expect(
                screen.queryByRole("heading", { name: "New rule" }),
            ).not.toBeInTheDocument();
        });
        const requests = fake.sent("POST", "/api/v1/notifications/rules");
        expect(requests).toHaveLength(1);
        expect(requests[0]?.headers.get("x-csrf-token")).toBe("csrf-value");
        expect(await requests[0]?.json()).toEqual({
            name: "missed-1",
            destination_id: DESTINATION_ID,
            event_kind: "job.replan_required",
            filter: { table_id: TABLE_ID },
            repeat_threshold: 3,
        });
        expect(screen.getByText("Created missed-1.")).toBeVisible();
        await waitFor(() => {
            expect(
                fake.sent("GET", "/api/v1/notifications/rules"),
            ).toHaveLength(2);
        });
    });

    it("keeps the opened rule revision when the list refreshes before save", async () => {
        fake.on(
            "GET",
            "/api/v1/notifications/rules",
            () => json(200, rulePage([rule()])),
            () => json(200, rulePage([rule({ revision: 3 })])),
        ).on("PATCH", "/api/v1/notifications/rules/{id}", () =>
            apiError(
                409,
                "revision_conflict",
                "PermanentFailure",
                "reload_and_retry",
            ),
        );
        const user = start("/notifications/rules");
        await user.click(
            await screen.findByRole("button", { name: "Edit failures" }),
        );
        const name = screen.getByLabelText("Rule name");
        await user.clear(name);
        await user.type(name, "failures-2");
        await act(async () => {
            await queryClient.invalidateQueries({
                queryKey: ["notifications", "rules"],
            });
        });
        await waitFor(() => {
            expect(
                fake.sent("GET", "/api/v1/notifications/rules"),
            ).toHaveLength(2);
        });
        await user.click(screen.getByRole("button", { name: "Save rule" }));
        await waitFor(() => {
            expect(
                fake.sent("PATCH", `/api/v1/notifications/rules/${RULE_ID}`),
            ).toHaveLength(1);
        });
        const request = fake.sent(
            "PATCH",
            `/api/v1/notifications/rules/${RULE_ID}`,
        )[0];
        expect(await request?.json()).toMatchObject({
            revision: 2,
            name: "failures-2",
        });
    });

    it("does not offer the event kind when editing a rule", async () => {
        const user = start("/notifications/rules");
        await user.click(
            await screen.findByRole("button", { name: "Edit failures" }),
        );
        const dialog = await screen.findByRole("dialog");
        expect(
            within(dialog).queryByRole("combobox", { name: "Event" }),
        ).not.toBeInTheDocument();
        expect(within(dialog).getByText("Job replan required")).toBeVisible();
        expect(
            within(dialog).getByText(/event kind cannot change/),
        ).toBeVisible();
    });
});
