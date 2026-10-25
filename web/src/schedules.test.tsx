import { QueryClientProvider } from "@tanstack/react-query";
import {
    fireEvent,
    render,
    screen,
    waitFor,
    within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { App } from "./App";
import { createQueryClient } from "./queryClient";
import { apiError, FakeApi, json, schema } from "./test/fakes";
import {
    JOB_ID,
    POLICY_ID,
    SCHEDULE_ID,
    SERVICE_ID,
    TABLE_ID,
    catalogs,
    policies,
    schedule,
    scheduleOccurrences,
    schedules,
    serviceIdentity,
    session,
    table,
} from "./test/fixtures";
import { shortId } from "./format";

let fake: FakeApi;
beforeEach(() => {
    fake = new FakeApi();
    vi.stubGlobal("fetch", fake.fetch);
    document.cookie = "bergsmith_csrf=csrf-value";
    fake.on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/schedules", () => json(200, schedules))
        .on("GET", "/api/v1/policies", () => json(200, policies))
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs))
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        )
        .on("GET", "/api/v1/service-identities", () =>
            json(
                200,
                schema("PrincipalPage", {
                    items: [serviceIdentity()],
                    next_cursor: null,
                }),
            ),
        );
});
afterEach(() => {
    expect(fake.unrouted).toEqual([]);
});

function start(path = "/schedules") {
    window.history.replaceState(null, "", path);
    const client = createQueryClient(0);
    render(
        <QueryClientProvider client={client}>
            <App />
        </QueryClientProvider>,
    );
    return userEvent.setup();
}

it("lists schedules by table with the policy, timing, and next instant", async () => {
    start();
    const link = await screen.findByRole("link", { name: "lake.sales.orders" });
    expect(link).toHaveAttribute("href", `/schedules/${SCHEDULE_ID}`);
    const row = screen.getByRole("row", { name: /lake\.sales\.orders/ });
    expect(within(row).getByText("nightly-compact")).toBeVisible();
    expect(within(row).getByText("0 2 * * *")).toBeVisible();
    expect(within(row).getAllByText(/Europe\/Berlin/).length).toBeGreaterThan(
        0,
    );
    expect(within(row).getByText("scheduler")).toBeVisible();
    expect(within(row).getByText("Enabled")).toBeVisible();
    expect(within(row).getByText("2")).toBeVisible();
});

it("creates a schedule with a wrapping window and the coordinator's zone", async () => {
    fake.on("POST", "/api/v1/schedules", () => json(201, schedule()));
    const user = start();
    await user.click(
        await screen.findByRole("button", { name: "New schedule" }),
    );
    await user.click(screen.getByRole("combobox", { name: "Policy" }));
    await user.click(screen.getByRole("option", { name: "nightly-compact" }));
    await user.click(
        screen.getByRole("combobox", { name: "Service identity" }),
    );
    await user.click(screen.getByRole("option", { name: "scheduler" }));
    await user.click(screen.getByRole("combobox", { name: "Table" }));
    await user.click(screen.getByRole("option", { name: "lake.sales.orders" }));
    await user.click(
        screen.getByLabelText("Only start runs inside a daily window"),
    );
    fireEvent.change(screen.getByLabelText("Window opens"), {
        target: { value: "22:00" },
    });
    fireEvent.change(screen.getByLabelText("Window closes"), {
        target: { value: "02:00" },
    });
    const zone = screen.getByLabelText<HTMLInputElement>("Time zone").value;
    await user.click(screen.getByRole("button", { name: "Create schedule" }));
    await waitFor(() => {
        expect(
            screen.queryByRole("heading", { name: "New schedule" }),
        ).not.toBeInTheDocument();
    });
    const request = fake.sent("POST", "/api/v1/schedules")[0];
    expect(request?.headers.get("x-csrf-token")).toBe("csrf-value");
    expect(await request?.json()).toEqual({
        policy_id: POLICY_ID,
        table_id: TABLE_ID,
        service_principal_id: SERVICE_ID,
        cron_expression: "0 2 * * *",
        time_zone: zone,
        window: { start_minute: 1320, end_minute: 120 },
    });
    expect(
        screen.getByText(/Created the schedule for lake\.sales\.orders/),
    ).toBeVisible();
});

it("shows the computed instants and the durable history", async () => {
    fake.on("GET", "/api/v1/schedules/{id}", () => json(200, schedule())).on(
        "GET",
        "/api/v1/schedules/{id}/occurrences",
        () => json(200, scheduleOccurrences),
    );
    start(`/schedules/${SCHEDULE_ID}`);
    expect(
        await screen.findByRole("heading", { name: "lake.sales.orders" }),
    ).toBeVisible();
    expect(screen.getByText("Next instants")).toBeVisible();
    // The two instants the coordinator computed are listed as times.
    const instants = within(
        screen.getByRole("list", { name: "Next instants" }),
    ).getAllByRole("listitem");
    expect(instants).toHaveLength(2);
    expect(screen.getByText("Admitted")).toBeVisible();
    expect(screen.getByText("Skipped")).toBeVisible();
    // The skip names its reason in prose, not as a storage token.
    expect(screen.getByText("Outside window")).toBeVisible();
    expect(screen.getByRole("link", { name: shortId(JOB_ID) })).toHaveAttribute(
        "href",
        `/jobs/${JOB_ID}`,
    );
    expect(
        screen.getByRole("table", { name: "Schedule evaluations" }),
    ).toBeVisible();
});

it("edits the timing against the revision it read", async () => {
    const updated = schedule({ revision: 3, cron_expression: "30 3 * * *" });
    fake.on("PATCH", "/api/v1/schedules/{id}", () => json(200, updated))
        .on(
            "GET",
            "/api/v1/schedules/{id}",
            () => json(200, schedule()),
            () => json(200, updated),
        )
        .on("GET", "/api/v1/schedules/{id}/occurrences", () =>
            json(200, scheduleOccurrences),
        );
    const user = start(`/schedules/${SCHEDULE_ID}`);
    await user.click(
        await screen.findByRole("button", { name: "Edit schedule" }),
    );
    const cron = screen.getByLabelText("Cron expression");
    await user.clear(cron);
    await user.type(cron, "30 3 * * *");
    await user.click(screen.getByRole("button", { name: "Save schedule" }));
    await waitFor(() => {
        expect(
            screen.queryByRole("heading", { name: /Edit schedule for/ }),
        ).not.toBeInTheDocument();
    });
    expect(
        await fake.sent("PATCH", `/api/v1/schedules/${SCHEDULE_ID}`)[0]?.json(),
    ).toEqual({ revision: 2, cron_expression: "30 3 * * *" });
    expect(
        screen.getByText("Saved the schedule for lake.sales.orders."),
    ).toBeVisible();
});

it("keeps edits and blocks the save when the schedule changed meanwhile", async () => {
    const current = schedule({ revision: 3, cron_expression: "0 4 * * *" });
    fake.on("PATCH", "/api/v1/schedules/{id}", () =>
        apiError(
            409,
            "revision_conflict",
            "PermanentFailure",
            "reload_and_retry",
        ),
    )
        .on(
            "GET",
            "/api/v1/schedules/{id}",
            () => json(200, schedule()),
            () => json(200, current),
        )
        .on("GET", "/api/v1/schedules/{id}/occurrences", () =>
            json(200, scheduleOccurrences),
        );
    const user = start(`/schedules/${SCHEDULE_ID}`);
    await user.click(
        await screen.findByRole("button", { name: "Edit schedule" }),
    );
    const cron = screen.getByLabelText("Cron expression");
    await user.clear(cron);
    await user.type(cron, "30 3 * * *");
    await user.click(screen.getByRole("button", { name: "Save schedule" }));
    expect(
        await screen.findByText(/changed while you were editing/),
    ).toBeVisible();
    expect(screen.getByLabelText("Cron expression")).toHaveValue("30 3 * * *");
    expect(
        screen.getByRole("button", { name: "Save schedule" }),
    ).toBeDisabled();
    await user.click(
        await screen.findByRole("button", { name: "Reload revision 3" }),
    );
    expect(screen.getByLabelText("Cron expression")).toHaveValue("0 4 * * *");
    expect(fake.sent("PATCH", `/api/v1/schedules/${SCHEDULE_ID}`)).toHaveLength(
        1,
    );
});

it("says a paused schedule will not run instead of naming its next instant", async () => {
    fake.on("GET", "/api/v1/schedules", () =>
        json(
            200,
            schema("MaintenanceSchedulePage", {
                items: [schedule({ enabled: false })],
                next_cursor: null,
            }),
        ),
    );
    start();
    const row = await screen.findByRole("row", { name: /lake\.sales\.orders/ });
    expect(within(row).getByText("Disabled")).toBeVisible();
    // The timing still names instants; the list must not imply they will run.
    expect(within(row).getByText("Paused")).toBeVisible();
});

it("explains a dormant expression and a paused schedule", async () => {
    fake.on("GET", "/api/v1/schedules/{id}", () =>
        json(
            200,
            schedule({
                enabled: false,
                cron_expression: "0 0 30 2 *",
                next_occurrences: [],
            }),
        ),
    ).on("GET", "/api/v1/schedules/{id}/occurrences", () =>
        json(
            200,
            schema("ScheduleOccurrencePage", { items: [], next_cursor: null }),
        ),
    );
    start(`/schedules/${SCHEDULE_ID}`);
    expect(await screen.findByText(/names no reachable instant/)).toBeVisible();
    expect(
        screen.queryByRole("list", { name: "Next instants" }),
    ).not.toBeInTheDocument();
    expect(screen.getByText("No evaluations yet.")).toBeVisible();
});

it("warns that a paused schedule admits none of its instants", async () => {
    fake.on("GET", "/api/v1/schedules/{id}", () =>
        json(200, schedule({ enabled: false })),
    ).on("GET", "/api/v1/schedules/{id}/occurrences", () =>
        json(
            200,
            schema("ScheduleOccurrencePage", { items: [], next_cursor: null }),
        ),
    );
    start(`/schedules/${SCHEDULE_ID}`);
    expect(
        await screen.findByText(/none of these will be admitted/),
    ).toBeVisible();
    expect(screen.getByRole("list", { name: "Next instants" })).toBeVisible();
});

it("disables the schedule and clears the window in one revision-checked edit", async () => {
    const updated = schedule({ revision: 3, enabled: false, window: null });
    fake.on("PATCH", "/api/v1/schedules/{id}", () => json(200, updated))
        .on(
            "GET",
            "/api/v1/schedules/{id}",
            () => json(200, schedule()),
            () => json(200, updated),
        )
        .on("GET", "/api/v1/schedules/{id}/occurrences", () =>
            json(200, scheduleOccurrences),
        );
    const user = start(`/schedules/${SCHEDULE_ID}`);
    await user.click(
        await screen.findByRole("button", { name: "Edit schedule" }),
    );
    await user.click(screen.getByLabelText("Enabled for scheduled runs"));
    await user.click(
        screen.getByLabelText("Only start runs inside a daily window"),
    );
    await user.click(screen.getByRole("button", { name: "Save schedule" }));
    await waitFor(() => {
        expect(
            screen.queryByRole("heading", { name: /Edit schedule for/ }),
        ).not.toBeInTheDocument();
    });
    expect(
        await fake.sent("PATCH", `/api/v1/schedules/${SCHEDULE_ID}`)[0]?.json(),
    ).toEqual({ revision: 2, enabled: false, window: null });
});

it("does not offer schedules to a session without organization configuration", async () => {
    fake.on("GET", "/auth/session", () => json(200, session("Viewer")));
    start();
    expect(await screen.findByText("Permission required")).toBeVisible();
    expect(
        screen.queryByRole("link", { name: "Schedules" }),
    ).not.toBeInTheDocument();
    expect(fake.sent("GET", "/api/v1/schedules")).toHaveLength(0);
});

it("explains which read grants a creator still needs", async () => {
    const limited = {
        ...session("Administrator"),
        permissions: session("Administrator").permissions.filter(
            (held) => held.permission === "schedules:manage",
        ),
    };
    fake.on("GET", "/auth/session", () => json(200, limited));
    start();
    expect(
        await screen.findByText(/Creating a schedule also needs/),
    ).toBeVisible();
    expect(
        screen.queryByRole("button", { name: "New schedule" }),
    ).not.toBeInTheDocument();
    // Reading the list still works with just schedules:manage.
    expect(
        await screen.findByRole("link", { name: "lake.sales.orders" }),
    ).toBeVisible();
});
