import { QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { App } from "./App";
import { createQueryClient } from "./queryClient";
import { apiError, FakeApi, json, schema } from "./test/fakes";
import { POLICY_ID, policies, policy, session } from "./test/fixtures";

const OTHER_POLICY_ID = "0190f3c4-0000-7000-8000-0000000000b2";

let fake: FakeApi;
beforeEach(() => {
    fake = new FakeApi();
    vi.stubGlobal("fetch", fake.fetch);
    document.cookie = "bergsmith_csrf=csrf-value";
    fake.on("GET", "/auth/session", () =>
        json(200, session("Administrator")),
    ).on("GET", "/api/v1/policies", () => json(200, policies));
});
afterEach(() => {
    expect(fake.unrouted).toEqual([]);
});

function start(path = "/policies") {
    window.history.replaceState(null, "", path);
    const client = createQueryClient(0);
    render(
        <QueryClientProvider client={client}>
            <App />
        </QueryClientProvider>,
    );
    return userEvent.setup();
}

it("lists policies with their settings and revision, and loads more", async () => {
    fake.on("GET", "/api/v1/policies", (request) =>
        json(
            200,
            schema("MaintenancePolicyPage", {
                items: new URL(request.url).searchParams.has("cursor")
                    ? [
                          policy({
                              id: OTHER_POLICY_ID,
                              name: "weekly-compact",
                              min_input_files: 12,
                              revision: 1,
                          }),
                      ]
                    : policies.items,
                next_cursor: new URL(request.url).searchParams.has("cursor")
                    ? null
                    : "next-page",
            }),
        ),
    );
    const user = start();
    const table = await screen.findByRole("table", {
        name: "Maintenance policies",
    });
    expect(within(table).getAllByText("nightly-compact")[0]).toBeVisible();
    expect(within(table).getByText("Binpack compaction")).toBeVisible();
    expect(within(table).getByText("256.0 MiB")).toBeVisible();
    expect(within(table).getByText("100.0 GiB")).toBeVisible();
    expect(within(table).getByText("Enabled")).toBeVisible();
    expect(within(table).getByText("3")).toBeVisible();
    await user.click(
        screen.getByRole("button", { name: "Load more policies" }),
    );
    expect(within(table).getAllByText("weekly-compact")[0]).toBeVisible();
    expect(within(table).getByText("12")).toBeVisible();
});

it("creates a policy from the planner defaults and refreshes the list", async () => {
    fake.on("POST", "/api/v1/policies", () =>
        json(201, policy({ name: "weekly-compact", revision: 1 })),
    );
    const user = start();
    await user.click(await screen.findByRole("button", { name: "New policy" }));
    expect(screen.getByLabelText("Target file size (bytes)")).toHaveValue(
        268_435_456,
    );
    expect(screen.getByLabelText("Minimum input files")).toHaveValue(5);
    expect(screen.getByLabelText("Run budget (bytes)")).toHaveValue(
        107_374_182_400,
    );
    expect(
        screen.getByRole("button", { name: "Create policy" }),
    ).toBeDisabled();
    await user.type(screen.getByLabelText("Policy name"), "weekly-compact");
    const minimum = screen.getByLabelText("Minimum input files");
    await user.clear(minimum);
    await user.type(minimum, "8");
    await user.click(screen.getByRole("button", { name: "Create policy" }));
    await waitFor(() => {
        expect(
            screen.queryByRole("heading", { name: "New policy" }),
        ).not.toBeInTheDocument();
    });
    const request = fake.sent("POST", "/api/v1/policies")[0];
    expect(request?.headers.get("x-csrf-token")).toBe("csrf-value");
    expect(await request?.json()).toEqual({
        name: "weekly-compact",
        target_file_size_bytes: 268_435_456,
        min_input_files: 8,
        max_input_bytes: 107_374_182_400,
    });
    expect(screen.getByText("Created weekly-compact.")).toBeVisible();
    // The list is read again so the new policy appears without a reload.
    expect(fake.sent("GET", "/api/v1/policies")).toHaveLength(2);
});

it("explains an empty list and cancels a create without a request", async () => {
    fake.on("GET", "/api/v1/policies", () =>
        json(
            200,
            schema("MaintenancePolicyPage", { items: [], next_cursor: null }),
        ),
    );
    const user = start();
    expect(await screen.findByText("No policies yet.")).toBeVisible();
    expect(
        screen.queryByRole("table", { name: "Maintenance policies" }),
    ).not.toBeInTheDocument();
    // Escape and Cancel both close without writing.
    await user.click(screen.getByRole("button", { name: "New policy" }));
    await user.keyboard("{Escape}");
    await waitFor(() => {
        expect(
            screen.queryByRole("heading", { name: "New policy" }),
        ).not.toBeInTheDocument();
    });
    await user.click(screen.getByRole("button", { name: "New policy" }));
    await user.click(await screen.findByRole("button", { name: "Cancel" }));
    expect(
        screen.queryByRole("heading", { name: "New policy" }),
    ).not.toBeInTheDocument();
    expect(fake.sent("POST", "/api/v1/policies")).toHaveLength(0);
});

it("edits only changed fields, naming the revision it read", async () => {
    const updated = policy({
        revision: 4,
        target_file_size_bytes: 536_870_912,
    });
    fake.on("PATCH", "/api/v1/policies/{id}", () => json(200, updated)).on(
        "GET",
        "/api/v1/policies",
        () => json(200, policies),
        () =>
            json(
                200,
                schema("MaintenancePolicyPage", {
                    items: [updated],
                    next_cursor: null,
                }),
            ),
    );
    const user = start();
    await user.click(
        await screen.findByRole("button", { name: "Edit nightly-compact" }),
    );
    expect(
        await screen.findByRole("heading", { name: "Edit nightly-compact" }),
    ).toBeVisible();
    expect(screen.getByRole("button", { name: "Save policy" })).toBeDisabled();
    const target = screen.getByLabelText("Target file size (bytes)");
    await user.clear(target);
    await user.type(target, "536870912");
    await user.click(screen.getByRole("button", { name: "Save policy" }));
    await waitFor(() => {
        expect(
            screen.queryByRole("heading", { name: "Edit nightly-compact" }),
        ).not.toBeInTheDocument();
    });
    const request = fake.sent("PATCH", `/api/v1/policies/${POLICY_ID}`)[0];
    expect(request?.headers.get("x-csrf-token")).toBe("csrf-value");
    expect(await request?.json()).toEqual({
        revision: 3,
        target_file_size_bytes: 536_870_912,
    });
    expect(screen.getByText("Saved nightly-compact.")).toBeVisible();
    const table = screen.getByRole("table", {
        name: "Maintenance policies",
    });
    expect(within(table).getByText("512.0 MiB")).toBeVisible();
    expect(within(table).getByText("4")).toBeVisible();
});

it("renames and disables a policy in one revision-checked edit", async () => {
    const updated = policy({
        revision: 4,
        name: "weekly-compact",
        enabled: false,
    });
    fake.on("PATCH", "/api/v1/policies/{id}", () => json(200, updated)).on(
        "GET",
        "/api/v1/policies",
        () => json(200, policies),
        () =>
            json(
                200,
                schema("MaintenancePolicyPage", {
                    items: [updated],
                    next_cursor: null,
                }),
            ),
    );
    const user = start();
    await user.click(
        await screen.findByRole("button", { name: "Edit nightly-compact" }),
    );
    const name = screen.getByLabelText("Policy name");
    await user.clear(name);
    await user.type(name, "weekly-compact");
    await user.click(screen.getByLabelText("Enabled for scheduled runs"));
    await user.click(screen.getByRole("button", { name: "Save policy" }));
    expect(await screen.findByText("Saved weekly-compact.")).toBeVisible();
    expect(
        await fake.sent("PATCH", `/api/v1/policies/${POLICY_ID}`)[0]?.json(),
    ).toEqual({
        revision: 3,
        name: "weekly-compact",
        enabled: false,
    });
    expect(screen.getByText("Disabled")).toBeVisible();
});

it("closes an edit without saving when the dialog is dismissed", async () => {
    const user = start();
    await user.click(
        await screen.findByRole("button", { name: "Edit nightly-compact" }),
    );
    await user.keyboard("{Escape}");
    await waitFor(() => {
        expect(
            screen.queryByRole("heading", { name: "Edit nightly-compact" }),
        ).not.toBeInTheDocument();
    });
    expect(fake.sent("PATCH", `/api/v1/policies/${POLICY_ID}`)).toHaveLength(0);
});

it("keeps the dialog open when the name is already taken", async () => {
    fake.on("PATCH", "/api/v1/policies/{id}", () =>
        apiError(
            409,
            "resource_already_exists",
            "PermanentFailure",
            "choose_another_name",
        ),
    );
    const user = start();
    await user.click(
        await screen.findByRole("button", { name: "Edit nightly-compact" }),
    );
    const name = screen.getByLabelText("Policy name");
    await user.clear(name);
    await user.type(name, "other-policy");
    await user.click(screen.getByRole("button", { name: "Save policy" }));
    expect(await screen.findByText("Resource already exists")).toBeVisible();
    expect(
        screen.getByRole("heading", { name: "Edit nightly-compact" }),
    ).toBeVisible();
    expect(screen.getByLabelText("Policy name")).toHaveValue("other-policy");
});

it("keeps edits and blocks the save when the policy changed meanwhile", async () => {
    const current = policy({
        revision: 4,
        target_file_size_bytes: 1_073_741_824,
    });
    fake.on("PATCH", "/api/v1/policies/{id}", () =>
        apiError(
            409,
            "revision_conflict",
            "PermanentFailure",
            "reload_and_retry",
        ),
    ).on(
        "GET",
        "/api/v1/policies",
        () => json(200, policies),
        () =>
            json(
                200,
                schema("MaintenancePolicyPage", {
                    items: [current],
                    next_cursor: null,
                }),
            ),
    );
    const user = start();
    await user.click(
        await screen.findByRole("button", { name: "Edit nightly-compact" }),
    );
    const target = screen.getByLabelText("Target file size (bytes)");
    await user.clear(target);
    await user.type(target, "536870912");
    await user.click(screen.getByRole("button", { name: "Save policy" }));
    expect(
        await screen.findByText(/changed while you were editing/),
    ).toBeVisible();
    // The rejected edits stay in the form; nothing is rebased behind them.
    expect(screen.getByLabelText("Target file size (bytes)")).toHaveValue(
        536_870_912,
    );
    expect(screen.getByRole("button", { name: "Save policy" })).toBeDisabled();
    // Reloading is explicit and takes the revision the list now shows.
    await user.click(
        await screen.findByRole("button", { name: "Reload revision 4" }),
    );
    expect(screen.getByLabelText("Target file size (bytes)")).toHaveValue(
        1_073_741_824,
    );
    expect(screen.getByRole("button", { name: "Save policy" })).toBeDisabled();
    expect(fake.sent("PATCH", `/api/v1/policies/${POLICY_ID}`)).toHaveLength(1);
});

it("does not offer policies to a session without organization configuration", async () => {
    fake.on("GET", "/auth/session", () => json(200, session("Viewer")));
    start("/policies");
    expect(await screen.findByText("Permission required")).toBeVisible();
    expect(
        screen.getByText(/Policies are organization configuration/),
    ).toBeVisible();
    expect(
        screen.queryByRole("link", { name: "Policies" }),
    ).not.toBeInTheDocument();
    expect(fake.sent("GET", "/api/v1/policies")).toHaveLength(0);
});
