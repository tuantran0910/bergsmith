// The delivery log pages: the bounded, newest-first list and one delivery's
// attempt history. Tests assert what an operator observes, including the
// filter query, cursor pagination, and the 404 the coordinator returns for a
// pruned row.

import { QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import { configure } from "@testing-library/react";

configure({ asyncUtilTimeout: 5_000 });
import userEvent from "@testing-library/user-event";

import { App } from "./App";
import type { Schemas } from "./api/client";
import { createQueryClient } from "./queryClient";
import { apiError, FakeApi, json, schema } from "./test/fakes";
import { JOB_ID, PRINCIPAL_ID, session } from "./test/fixtures";

const DELIVERY_ID = "0190f3c4-0000-7000-8000-0000000000f1";
const OTHER_DELIVERY_ID = "0190f3c4-0000-7000-8000-0000000000f2";
const DESTINATION_ID = "0190f3c4-0000-7000-8000-0000000000e1";
const RULE_ID = "0190f3c4-0000-7000-8000-0000000000e2";

const destinations = schema("NotificationDestinationPage", {
    items: [
        {
            id: DESTINATION_ID,
            name: "ops-slack",
            kind: "slack",
            signing_secret_ref: null,
            enabled: true,
            revision: 1,
            created_by: PRINCIPAL_ID,
            created_at: "2026-10-01T00:00:00Z",
            updated_at: "2026-10-01T00:00:00Z",
            url_ref: "slack-runbook-url",
        },
    ],
    next_cursor: null,
});

const rules = schema("NotificationRulePage", {
    items: [
        {
            id: RULE_ID,
            name: "failed-jobs",
            destination_id: DESTINATION_ID,
            event_kind: "job.failed",
            filter: null,
            repeat_threshold: null,
            enabled: true,
            revision: 1,
            created_by: PRINCIPAL_ID,
            created_at: "2026-10-01T00:00:00Z",
            updated_at: "2026-10-01T00:00:00Z",
        },
    ],
    next_cursor: null,
});

function detail(
    overrides: Partial<Schemas["NotificationDeliveryDetail"]> = {},
): Schemas["NotificationDeliveryDetail"] {
    return schema("NotificationDeliveryDetail", {
        id: DELIVERY_ID,
        rule_id: RULE_ID,
        destination_id: DESTINATION_ID,
        destination_name: "ops-slack",
        destination_kind: "slack",
        event_kind: "job.failed",
        occurred_at: "2026-10-26T12:00:00Z",
        summary: "Job 0190f3c4 failed",
        resource: { kind: "job", id: JOB_ID, label: "nightly-compact" },
        reason: "job_failed",
        state: "delivered",
        attempt_count: 2,
        next_attempt_at: "2026-10-26T12:05:00Z",
        last_error: null,
        created_at: "2026-10-26T12:00:00Z",
        updated_at: "2026-10-26T12:01:00Z",
        attempts: [],
        ...overrides,
    });
}

/** A list row carries the same fields as the detail without its attempts. */
function listed(
    record: Schemas["NotificationDeliveryDetail"],
): Schemas["NotificationDelivery"] {
    return schema("NotificationDelivery", {
        id: record.id,
        rule_id: record.rule_id,
        destination_id: record.destination_id,
        destination_name: record.destination_name,
        destination_kind: record.destination_kind,
        event_kind: record.event_kind,
        occurred_at: record.occurred_at,
        summary: record.summary,
        resource: record.resource,
        reason: record.reason,
        state: record.state,
        attempt_count: record.attempt_count,
        next_attempt_at: record.next_attempt_at,
        last_error: record.last_error,
        created_at: record.created_at,
        updated_at: record.updated_at,
    });
}

function page(
    items: readonly Schemas["NotificationDelivery"][],
    nextCursor: string | null = null,
): Schemas["NotificationDeliveryPage"] {
    return schema("NotificationDeliveryPage", {
        items: [...items],
        next_cursor: nextCursor,
    });
}

let fake: FakeApi;

beforeEach(() => {
    fake = new FakeApi();
    vi.stubGlobal("fetch", fake.fetch);
    document.cookie = "bergsmith_csrf=csrf-value";
    fake.on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/notifications/destinations", () =>
            json(200, destinations),
        )
        .on("GET", "/api/v1/notifications/rules", () => json(200, rules));
});

afterEach(() => {
    expect(fake.unrouted).toEqual([]);
});

function start(path = "/notifications/deliveries") {
    window.history.replaceState(null, "", path);
    const client = createQueryClient(0);
    render(
        <QueryClientProvider client={client}>
            <App />
        </QueryClientProvider>,
    );
    return userEvent.setup();
}

it("lists the delivery summary, destination, state, and attempt count", async () => {
    fake.on("GET", "/api/v1/notifications/deliveries", () =>
        json(200, page([listed(detail())])),
    );
    start();
    const table = await screen.findByRole("table", {
        name: "Notification deliveries",
    });
    const row = within(table).getByRole("row", {
        name: /Job 0190f3c4 failed/,
    });
    expect(within(row).getByText("ops-slack")).toBeVisible();
    expect(within(row).getByText("Job failed")).toBeVisible();
    expect(within(row).getByText("nightly-compact")).toBeVisible();
    expect(within(row).getByText("Delivered")).toBeVisible();
    expect(within(row).getByText("2")).toBeVisible();
    expect(
        within(row).getByRole("link", { name: /Open delivery/ }),
    ).toHaveAttribute("href", `/notifications/deliveries/${DELIVERY_ID}`);
});

it("filters by state and sends the state to the coordinator", async () => {
    fake.on("GET", "/api/v1/notifications/deliveries", (request) =>
        json(
            200,
            new URL(request.url).searchParams.get("state") === "exhausted"
                ? page([
                      listed(
                          detail({
                              id: OTHER_DELIVERY_ID,
                              summary: "Scheduled job exhausted",
                              state: "exhausted",
                              attempt_count: 5,
                              last_error: "attempts_exhausted",
                              reason: null,
                          }),
                      ),
                  ])
                : page([listed(detail())]),
        ),
    );
    const user = start();
    await screen.findByRole("table", {
        name: "Notification deliveries",
    });
    await user.click(screen.getByRole("combobox", { name: "State" }));
    await user.click(await screen.findByRole("option", { name: "Exhausted" }));
    expect(await screen.findByText("Scheduled job exhausted")).toBeVisible();
    await waitFor(() => {
        expect(
            fake
                .sent("GET", "/api/v1/notifications/deliveries")
                .some(
                    (request) =>
                        new URL(request.url).searchParams.get("state") ===
                        "exhausted",
                ),
        ).toBe(true);
    });
    expect(screen.getByRole("button", { name: "Clear filters" })).toBeVisible();
});

it("loads the second page through next_cursor", async () => {
    fake.on("GET", "/api/v1/notifications/deliveries", (request) => {
        const continuation = new URL(request.url).searchParams.has("cursor");
        return json(
            200,
            continuation
                ? page([
                      listed(
                          detail({
                              id: OTHER_DELIVERY_ID,
                              summary: "Scheduled job failed",
                          }),
                      ),
                  ])
                : page([listed(detail())], "next-page"),
        );
    });
    const user = start();
    const table = await screen.findByRole("table", {
        name: "Notification deliveries",
    });
    expect(within(table).getByText("Job 0190f3c4 failed")).toBeVisible();
    await user.click(
        screen.getByRole("button", { name: "Load more deliveries" }),
    );
    expect(await screen.findByText("Scheduled job failed")).toBeVisible();
    await waitFor(() => {
        expect(
            fake
                .sent("GET", "/api/v1/notifications/deliveries")
                .some(
                    (request) =>
                        new URL(request.url).searchParams.get("cursor") ===
                        "next-page",
                ),
        ).toBe(true);
    });
});

it("shows attempt outcomes, statuses, and stable reasons", async () => {
    fake.on("GET", "/api/v1/notifications/deliveries/{id}", () =>
        json(
            200,
            detail({
                attempts: [
                    {
                        attempt: 1,
                        finished_at: "2026-10-26T12:00:30Z",
                        outcome: "failed",
                        status: 502,
                        reason: "destination_unavailable",
                    },
                    {
                        attempt: 2,
                        finished_at: "2026-10-26T12:01:00Z",
                        outcome: "delivered",
                        status: 200,
                        reason: null,
                    },
                    {
                        attempt: 3,
                        finished_at: "2026-10-26T12:02:00Z",
                        outcome: "rejected",
                        status: null,
                        reason: "signature_rejected",
                    },
                ],
            }),
        ),
    );
    start(`/notifications/deliveries/${DELIVERY_ID}`);
    expect(
        await screen.findByRole("heading", {
            name: "Job 0190f3c4 failed",
        }),
    ).toBeVisible();
    const table = await screen.findByRole("table", {
        name: "Delivery attempts",
    });
    expect(within(table).getByText("Failed")).toBeVisible();
    expect(within(table).getByText("Delivered")).toBeVisible();
    expect(within(table).getByText("Rejected")).toBeVisible();
    expect(within(table).getByText("502")).toBeVisible();
    expect(within(table).getByText("-")).toBeVisible();
    expect(within(table).getByText("Destination unavailable")).toBeVisible();
    expect(screen.getByText(/response body is never stored/i)).toBeVisible();
});

it("explains an empty attempt history", async () => {
    fake.on("GET", "/api/v1/notifications/deliveries/{id}", () =>
        json(200, detail({ attempt_count: 0 })),
    );
    start(`/notifications/deliveries/${DELIVERY_ID}`);
    expect(await screen.findByText("No attempts recorded yet.")).toBeVisible();
});

it("shows the server reason when the delivery is missing", async () => {
    fake.on("GET", "/api/v1/notifications/deliveries/{id}", () =>
        apiError(
            404,
            "resource_not_found",
            "PermanentFailure",
            "Check the delivery ID",
        ),
    );
    start(`/notifications/deliveries/${DELIVERY_ID}`);
    const alert = await screen.findByRole("alert");
    expect(within(alert).getByText("Resource not found")).toBeVisible();
    expect(within(alert).getByText(/Check the delivery ID/)).toBeVisible();
});

it("requires an organization grant before reading the log", async () => {
    fake.on("GET", "/auth/session", () => json(200, session("Viewer")));
    start();
    expect(await screen.findByText("Permission required")).toBeVisible();
    expect(screen.getByText(/organization-wide grant/)).toBeVisible();
    expect(fake.sent("GET", "/api/v1/notifications/deliveries")).toHaveLength(
        0,
    );
});
