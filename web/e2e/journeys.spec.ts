// The M4 journeys in a real browser, under the coordinator's CSP, with axe
// checking each page (BS-021.T4).

import { schema } from "../src/test/fakes";
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
    session,
    table,
    tableSnapshots,
    tableDefinition,
    overview,
    tableMaintenance,
    tableStatistics,
} from "../src/test/fixtures";
import {
    type FakeCoordinator,
    eventStream,
    expect,
    expectAccessible,
    json,
    test,
} from "./coordinator";

const WORKER_ID = "0190f3c4-0000-7000-8000-0000000000f1";

/** The routes every signed-in page reads. */
function signedIn(coordinator: FakeCoordinator, role = "Operator") {
    coordinator
        .on("GET", "/auth/session", () => json(200, session(role)))
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs))
        .on("GET", "/api/v1/tables/{id}", () => json(200, table))
        // The landing page reads the overview; every shell journey visits it.
        .on("GET", "/api/v1/overview", () => json(200, overview));
}

test("signs in through the organization and returns to the requested page", async ({
    page,
    coordinator,
}) => {
    let signedInYet = false;
    coordinator
        .on("GET", "/auth/session", () =>
            signedInYet
                ? json(200, session())
                : json(401, { error: "no_session" }),
        )
        // The coordinator would redirect to the identity provider. A redirect
        // answered by interception escapes the next interception, so the
        // test replays the provider's return to `return_to` itself.
        .on("GET", "/auth/login", () => {
            signedInYet = true;
            return { status: 200, body: "", contentType: "text/html" };
        })
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs))
        .on("GET", "/api/v1/jobs", () =>
            json(200, schema("JobPage", { items: [], next_cursor: null })),
        );

    await page.goto("/jobs");
    const organization = page.getByLabel("Organization");
    await expect(organization).toBeVisible();
    await expectAccessible(page);
    await organization.fill(" acme ");
    await page.getByRole("button", { name: "Sign in" }).click();
    await page.waitForURL("/auth/login?organization=acme&return_to=%2Fjobs");
    await page.goto("/jobs");

    await expect(page.getByText("No jobs are visible to you.")).toBeVisible();
    await expectAccessible(page);
});

test("browses to a table, previews it, and submits a job", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        )
        .on("POST", "/api/v1/previews", () => json(200, plannedPreview))
        .on("POST", "/api/v1/jobs", () =>
            json(
                202,
                schema("AcceptedJob", {
                    operation_id: JOB_ID,
                    job_id: JOB_ID,
                    resource_url: `/api/v1/jobs/${JOB_ID}`,
                }),
            ),
        )
        .on("GET", "/api/v1/jobs/{id}", () => json(200, job("Planning")))
        .on("GET", "/api/v1/jobs/{id}/events", () =>
            eventStream([event(1, "Planning", null)]),
        );

    await page.goto("/tables");
    await expectAccessible(page);
    await page.getByRole("link", { name: /^orders/ }).click();
    await expect(
        page.getByRole("heading", { name: "lake.sales.orders" }),
    ).toBeVisible();
    await page.getByText("Technical details").click();
    await expect(
        page.getByRole("button", { name: "Copy table UUID" }),
    ).toBeVisible();
    await expectAccessible(page);

    await page.getByRole("button", { name: "Preview maintenance" }).click();
    const preview = page.getByRole("region", { name: "Preview" });
    await expect(preview.getByText("Maintenance would run")).toBeVisible();
    await expect(preview.getByText("9007199254740993")).toBeVisible();
    await expectAccessible(page);

    await page.getByRole("button", { name: "Run maintenance" }).click();
    await expect(page).toHaveURL(`/jobs/${JOB_ID}`);
    await expect(page.getByRole("list", { name: "Job events" })).toBeVisible();
    await expectAccessible(page);

    expect(coordinator.sent("POST", "/api/v1/previews")[0]?.body).toEqual({
        table_id: TABLE_ID,
    });
    const submitted = coordinator.sent("POST", "/api/v1/jobs")[0];
    expect(submitted?.body).toEqual({ table_id: TABLE_ID });
    expect(submitted?.headers["x-csrf-token"]).toBe("csrf-value");
    expect(submitted?.headers["idempotency-key"]).toMatch(/^[0-9a-f-]{36}$/);
});

test("shows a table's statistics on an addressable tab", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator.on("GET", "/api/v1/tables/{id}/statistics", () =>
        json(200, tableStatistics),
    );

    await page.goto(`/tables/${TABLE_ID}`);
    await page.getByRole("tab", { name: "Statistics" }).click();

    await expect(page).toHaveURL(
        new RegExp(`/tables/${TABLE_ID}\\?tab=statistics$`),
    );
    await expect(
        page.getByRole("img", { name: /Data file sizes/ }),
    ).toBeVisible();
    await expect(
        page.getByText("+2.0 KiB since the previous sample"),
    ).toBeVisible();
    await expect(
        page.getByRole("columnheader", { name: "Rows" }),
    ).toBeVisible();
    await expect(page.getByText("day=2026-10-01")).toBeVisible();
    const partitions = page.getByRole("region", {
        name: "Busiest partitions",
    });
    await expect(
        partitions.getByRole("columnheader", { name: "Delete files" }),
    ).toBeVisible();
    await expect(
        partitions.getByRole("cell", { name: "1", exact: true }),
    ).toBeVisible();
    await expectAccessible(page);

    // The overview is still one click away and keeps its own address.
    await page.getByRole("tab", { name: "Overview" }).click();
    await expect(page).toHaveURL(`/tables/${TABLE_ID}`);
    await expect(
        page.getByRole("button", { name: "Preview maintenance" }),
    ).toBeVisible();
});

test("shows a table's definition on an addressable tab", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator.on("GET", "/api/v1/tables/{id}/definition", () =>
        json(200, tableDefinition),
    );

    await page.goto(`/tables/${TABLE_ID}`);
    await page.getByRole("tab", { name: "Definition" }).click();

    await expect(page).toHaveURL(
        new RegExp(`/tables/${TABLE_ID}\\?tab=definition$`),
    );
    await expect(page.getByRole("heading", { name: "Schema" })).toBeVisible();
    await expect(
        page.getByRole("region", { name: "Schema fields" }),
    ).toBeVisible();
    await expect(page.getByText("row key")).toBeVisible();
    await expect(
        page.getByRole("region", { name: "Spec 0 fields" }),
    ).toBeVisible();
    await expect(page.getByText("write.format.default")).toBeVisible();
    await expectAccessible(page);
});

test("shows a table's maintenance history on an addressable tab", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator.on("GET", "/api/v1/tables/{id}/maintenance", () =>
        json(200, tableMaintenance),
    );

    await page.goto(`/tables/${TABLE_ID}`);
    await page.getByRole("tab", { name: "Maintenance" }).click();

    await expect(page).toHaveURL(
        new RegExp(`/tables/${TABLE_ID}\\?tab=maintenance$`),
    );
    await expect(page.getByRole("heading", { name: "Impact" })).toBeVisible();
    const history = page.getByRole("region", { name: "Maintenance history" });
    await expect(
        history.getByRole("columnheader", { name: "Bytes rewritten" }),
    ).toBeVisible();
    await expect(history.getByText("1 min 5 s")).toBeVisible();
    await expect(history.getByText("100 B")).toBeVisible();
    await expect(history.getByText("Stopped: replan required")).toBeVisible();
    await expectAccessible(page);
});

test("shows a table's snapshot timeline on an addressable tab", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator.on("GET", "/api/v1/tables/{id}/snapshots", () =>
        json(200, tableSnapshots),
    );

    await page.goto(`/tables/${TABLE_ID}`);
    await page.getByRole("tab", { name: "Snapshots" }).click();

    await expect(page).toHaveURL(
        new RegExp(`/tables/${TABLE_ID}\\?tab=snapshots$`),
    );
    await expect(
        page.getByRole("img", { name: /Commits per hour/ }),
    ).toBeVisible();
    await expect(page.getByText("+2 data files, +40 rows")).toBeVisible();
    await expect(
        page.getByRole("link", { name: "job 0000000a" }),
    ).toHaveAttribute("href", `/jobs/${JOB_ID}`);
    await expectAccessible(page);

    await page.getByRole("tab", { name: "Overview" }).click();
    await expect(page).toHaveURL(`/tables/${TABLE_ID}`);
});

test("follows a job's events to success, then closes the stream", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator
        .on(
            "GET",
            "/api/v1/jobs/{id}",
            () => json(200, job("Running")),
            () =>
                json(
                    200,
                    job("Succeeded", {
                        publication_started: true,
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
        )
        .on("GET", "/api/v1/jobs/{id}/events", () =>
            eventStream([
                event(1, "Queued", null),
                event(2, "Running", "Queued"),
                event(3, "ReadyToCommit", "Running"),
                event(4, "Succeeded", "Committing"),
            ]),
        );

    await page.goto(`/jobs/${JOB_ID}`);

    await expect(
        page.getByRole("heading", { name: "lake.sales.orders" }),
    ).toBeVisible();
    await expect(
        page
            .getByRole("navigation", { name: "Breadcrumb" })
            .getByText(`Job ${JOB_ID.slice(-8)}`, { exact: true }),
    ).toBeVisible();
    await expect(
        page.getByRole("button", { name: "Copy job ID" }),
    ).toBeVisible();
    await expect(page.getByText("Confirmed", { exact: true })).toBeVisible();
    await expect(
        page.getByRole("heading", { name: "Maintenance statistics" }),
    ).toBeVisible();
    await expect(
        page.getByText("Input files rewritten").locator(".."),
    ).toContainText("8");
    await expect(
        page.getByText(/Accepted outputs were committed to the table/),
    ).toBeVisible();
    await expect(page.getByText("The job has ended.")).toBeVisible();
    await expect(
        page.getByRole("list", { name: "Job events" }).getByRole("listitem"),
    ).toHaveCount(4);
    await expect(page.getByRole("button", { name: "Cancel job" })).toHaveCount(
        0,
    );
    await expect(
        page.getByRole("link", { name: "lake.sales.orders" }),
    ).toHaveAttribute("href", `/tables/${TABLE_ID}`);
    await expectAccessible(page);
    expect(
        coordinator.sent("GET", `/api/v1/jobs/${JOB_ID}/events`),
    ).toHaveLength(1);
});

test("cancels a job through the confirmation dialog", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator
        .on("GET", "/api/v1/jobs/{id}", () => json(200, job("Queued")))
        .on("GET", "/api/v1/jobs/{id}/events", () =>
            eventStream([event(1, "Queued", null)]),
        )
        .on("POST", "/api/v1/jobs/{id}/cancel", () =>
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

    await page.goto(`/jobs/${JOB_ID}`);
    await page.getByRole("button", { name: "Cancel job" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toBeVisible();
    // Let the opening transition finish before axe measures controls.
    await expect(dialog).toHaveCSS("opacity", "1");
    await expectAccessible(page);
    await dialog
        .getByLabel("Cancellation reason (optional)")
        .fill(" wrong table ");
    await dialog.getByRole("button", { name: "Request cancellation" }).click();

    await expect(
        dialog.getByText("Cancellation accepted; the job is stopping."),
    ).toBeVisible();
    await expectAccessible(page);
    const request = coordinator.sent("POST", `/api/v1/jobs/${JOB_ID}/cancel`);
    expect(request).toHaveLength(1);
    expect(request[0]?.body).toEqual({ reason: "wrong table" });
    expect(request[0]?.headers["x-csrf-token"]).toBe("csrf-value");
});

test("retries a failed job with a fresh plan and opens the new job", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator
        .on("GET", "/api/v1/jobs/{id}", (_sent, params) =>
            params["id"] === RETRY_JOB_ID
                ? json(
                      200,
                      job("Planning", {
                          id: RETRY_JOB_ID,
                          retry_of_job_id: JOB_ID,
                      }),
                  )
                : json(200, job("Failed", { state_reason: "plan_rejected" })),
        )
        .on("GET", "/api/v1/jobs/{id}/events", (_sent, params) =>
            params["id"] === RETRY_JOB_ID
                ? eventStream([event(1, "Planning", null)])
                : eventStream([
                      event(1, "Planning", null),
                      event(2, "Failed", "Planning"),
                  ]),
        )
        .on("POST", "/api/v1/jobs/{id}/retry", () =>
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

    await page.goto(`/jobs/${JOB_ID}`);
    await expect(page.getByText(/Plan rejected/)).toBeVisible();
    await expectAccessible(page);
    await page.getByRole("button", { name: "Retry with a fresh plan" }).click();

    await expect(page).toHaveURL(`/jobs/${RETRY_JOB_ID}`);
    await expect(
        page.getByRole("link", { name: `Job ${JOB_ID.slice(-8)}` }),
    ).toHaveAttribute("href", `/jobs/${JOB_ID}`);
    const retry = coordinator.sent("POST", `/api/v1/jobs/${JOB_ID}/retry`);
    expect(retry).toHaveLength(1);
    expect(retry[0]?.headers["idempotency-key"]).toMatch(/^[0-9a-f-]{36}$/);
    await expect
        .poll(
            () =>
                coordinator.sent("GET", `/api/v1/jobs/${RETRY_JOB_ID}/events`)
                    .length,
        )
        .toBe(1);
});

test("shows audit history and workers to an administrator", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator, "Administrator");
    coordinator
        .on("GET", "/api/v1/audit", () =>
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
        )
        .on("GET", "/api/v1/workers", () =>
            json(
                200,
                schema("WorkerSessionPage", {
                    items: [
                        {
                            id: WORKER_ID,
                            principal_id: CATALOG_ID,
                            pool: "default",
                            worker_name: "worker-1",
                            build_version: "0.1.0",
                            capabilities: ["rewrite"],
                            memory_bytes: 4 * 1024 ** 3,
                            cpu_millis: 2000,
                            status: "live",
                            registered_at: "2026-10-21T09:00:00Z",
                            lease_expires_at: "2026-10-21T10:01:00Z",
                            running_attempt_id: null,
                        },
                    ],
                    next_cursor: null,
                }),
            ),
        );

    await page.goto("/audit");
    await expect(page.getByText("session.denied")).toBeVisible();
    await expect(
        page.getByRole("table").getByText("Alice", { exact: true }),
    ).toHaveAttribute("title", PRINCIPAL_ID);
    await expect(
        page.getByRole("link", { name: `Job ${JOB_ID.slice(-8)}` }),
    ).toBeVisible();
    await expect(page.getByRole("link", { name: "Audit" })).toHaveAttribute(
        "aria-current",
        "page",
    );
    await expectAccessible(page);

    await page.getByRole("link", { name: "Workers" }).click();
    await expect(page.getByText("worker-1")).toBeVisible();
    await expectAccessible(page);
});

test("lands on the overview and opens the tables", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator.on("GET", "/api/v1/tables", () =>
        json(200, schema("TablePage", { items: [table], next_cursor: null })),
    );

    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Overview" })).toBeVisible();
    await expect(
        page.getByRole("heading", { name: "Needs attention" }),
    ).toBeVisible();
    await expect(page.getByText("Publication unresolved")).toBeVisible();
    await expect(
        page.getByRole("link", { name: /^Replan required/ }),
    ).toBeVisible();
    await expect(
        page.getByRole("img", { name: /Jobs in the last day/ }),
    ).toBeVisible();
    await expectAccessible(page);

    await page.getByRole("link", { name: /Browse tables and start/ }).click();
    await expect(page).toHaveURL("/tables");
    await expect(
        page
            .getByRole("navigation", { name: "Breadcrumb" })
            .getByRole("link", { name: "Overview" }),
    ).toBeVisible();
    await expect(page.getByRole("link", { name: /^orders/ })).toBeVisible();
    await expectAccessible(page);
});

test("filters the table list and keeps the filter in the address", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
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
    coordinator.on("GET", "/api/v1/tables", () =>
        json(
            200,
            schema("TablePage", {
                items: [table, other],
                next_cursor: null,
            }),
        ),
    );

    await page.goto("/tables");
    await expect(page.getByRole("link", { name: /^orders/ })).toBeVisible();
    await page.getByRole("searchbox", { name: "Search tables" }).fill("events");

    await expect(page.getByRole("link", { name: /^orders/ })).toBeHidden();
    await expect(page.getByRole("link", { name: /^events/ })).toBeVisible();
    await expect(page).toHaveURL(/q=events/);
    await expect(page.getByText("1 of 2 tables")).toBeVisible();
    await expectAccessible(page);

    await page.getByRole("button", { name: "Clear filters" }).click();
    await expect(page.getByRole("link", { name: /^orders/ })).toBeVisible();
    await expect(page).not.toHaveURL(/q=/);
});

test("switches and remembers table density", async ({ page, coordinator }) => {
    signedIn(coordinator);
    coordinator.on("GET", "/api/v1/tables", () =>
        json(200, schema("TablePage", { items: [table], next_cursor: null })),
    );

    await page.goto("/tables");
    await expect(page.getByRole("link", { name: /^orders/ })).toBeVisible();
    const comfortable = await page.getByRole("row").nth(1).boundingBox();

    await page.getByRole("button", { name: "Compact density" }).click();
    await expect(page.locator("html")).toHaveAttribute(
        "data-density",
        "compact",
    );
    const compact = await page.getByRole("row").nth(1).boundingBox();
    if (compact === null || comfortable === null) {
        throw new Error("the table row has no bounding box");
    }
    expect(compact.height).toBeLessThan(comfortable.height);

    await page.reload();
    await expect(page.getByRole("link", { name: /^orders/ })).toBeVisible();
    await expect(page.locator("html")).toHaveAttribute(
        "data-density",
        "compact",
    );

    // The drawer and the rail share one preference, not two copies.
    await page.setViewportSize({ width: 390, height: 844 });
    await page.getByRole("button", { name: "Open navigation" }).click();
    const drawer = page.getByRole("dialog");
    await expect(
        drawer.getByRole("button", { name: "Compact density" }),
    ).toHaveAttribute("aria-pressed", "true");
    await drawer.getByRole("button", { name: "Comfortable density" }).click();
    await expect(page.locator("html")).not.toHaveAttribute("data-density");
    await drawer.getByRole("button", { name: "Close navigation" }).click();
    await expect(drawer).toBeHidden();
    await page.setViewportSize({ width: 1440, height: 900 });
    await expect(
        page.getByRole("button", { name: "Comfortable density" }),
    ).toHaveAttribute("aria-pressed", "true");
    await expectAccessible(page);
});

test("shows the scroll hint only when the table overflows", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator.on("GET", "/api/v1/tables", () =>
        json(200, schema("TablePage", { items: [table], next_cursor: null })),
    );

    await page.setViewportSize({ width: 1440, height: 900 });
    await page.goto("/tables");
    await expect(page.getByRole("link", { name: /^orders/ })).toBeVisible();
    const hint = page.getByText("Scroll horizontally for more columns.");
    await expect(hint).toBeHidden();

    await page.setViewportSize({ width: 768, height: 900 });
    await expect(hint).toBeVisible();

    await page.setViewportSize({ width: 1440, height: 900 });
    await expect(hint).toBeHidden();
});

test("preloads the tables chunk on intent", async ({ page, coordinator }) => {
    signedIn(coordinator);
    coordinator
        .on("GET", "/api/v1/jobs", () =>
            json(200, schema("JobPage", { items: [], next_cursor: null })),
        )
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        );

    await page.goto("/jobs");
    await expect(page.getByText("No jobs are visible to you.")).toBeVisible();
    const preloaded = page.waitForRequest(/TablesPage-.*\.js/);
    await page.getByRole("link", { name: "Tables", exact: true }).hover();
    await preloaded;
});

test("stays accessible in the narrow layout", async ({ page, coordinator }) => {
    signedIn(coordinator, "Administrator");
    coordinator
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        )
        .on("GET", "/api/v1/jobs", () =>
            json(200, schema("JobPage", { items: [], next_cursor: null })),
        );
    await page.setViewportSize({ width: 390, height: 844 });

    await page.goto("/tables");
    await expect(page.getByRole("link", { name: /^orders/ })).toBeVisible();
    // The rail is out of the way; the drawer carries the same groups.
    await expect(page.getByRole("navigation", { name: "Main" })).toBeHidden();
    await page.getByRole("button", { name: "Open navigation" }).click();
    const drawer = page.getByRole("dialog");
    await expect(drawer).toBeVisible();
    await expect(drawer).toHaveCSS("opacity", "1");
    await expect(
        drawer.getByRole("heading", { name: "Administration" }),
    ).toBeVisible();
    await expect(
        drawer.getByRole("link", { name: "Tables", exact: true }),
    ).toHaveAttribute("aria-current", "page");
    await expectAccessible(page);
    await drawer.getByRole("link", { name: "Jobs" }).click();
    await expect(drawer).toBeHidden();
    await expect(page).toHaveURL("/jobs");
    await expect(page.locator("main")).toBeFocused();

    // The skip link is the first tab stop and reaches the main landmark.
    await page.goto("/tables");
    await expect(page.getByRole("link", { name: /^orders/ })).toBeVisible();
    await page.keyboard.press("Tab");
    const skip = page.getByRole("link", { name: "Skip to main content" });
    await expect(skip).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(page.locator("main")).toBeFocused();
    await expectAccessible(page);
});

test("shows a status while a page chunk loads", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        )
        .on("GET", "/api/v1/jobs", () =>
            json(200, schema("JobPage", { items: [], next_cursor: null })),
        );
    await page.route("**/assets/JobsPage-*.js", async (route) => {
        await new Promise((resolve) => setTimeout(resolve, 600));
        // The coordinator fixture owns the response for this fake origin.
        await route.fallback();
    });

    await page.goto("/tables");
    await expect(page.getByRole("link", { name: /^orders/ })).toBeVisible();
    await page.getByRole("link", { name: "Jobs", exact: true }).click();
    await expect(page.getByRole("status")).toContainText("Loading page…");
    await expect(page.getByRole("heading", { name: "Jobs" })).toBeVisible();
});

for (const width of [320, 768, 1440]) {
    test(`keeps the shell usable at ${width} px`, async ({
        page,
        coordinator,
    }) => {
        signedIn(coordinator, "Administrator");
        await page.setViewportSize({ width, height: 844 });
        await page.goto("/");
        await expect(
            page.getByRole("heading", { name: "Overview" }),
        ).toBeVisible();

        const rail = page.getByRole("navigation", { name: "Main" });
        const trigger = page.getByRole("button", { name: "Open navigation" });
        if (width >= 768) {
            await expect(rail).toBeVisible();
            await expect(trigger).toBeHidden();
            await expect(
                rail.getByRole("link", { name: "Overview", exact: true }),
            ).toHaveAttribute("aria-current", "page");
        } else {
            await expect(rail).toBeHidden();
            await trigger.click();
            const drawer = page.getByRole("dialog");
            await expect(drawer).toBeVisible();
            await expect(drawer).toHaveCSS("opacity", "1");
            await expect(
                drawer.getByRole("link", { name: "Overview", exact: true }),
            ).toHaveAttribute("aria-current", "page");
            await expectAccessible(page);
            await page.keyboard.press("Escape");
            await expect(drawer).toBeHidden();
        }

        const overflow = await page.evaluate(
            () =>
                document.documentElement.scrollWidth -
                document.documentElement.clientWidth,
        );
        expect(overflow).toBeLessThanOrEqual(0);
        await expectAccessible(page);
    });
}

test("the policy blocks inline styles and string script sinks", async ({
    page,
    coordinator,
    problems,
}) => {
    signedIn(coordinator);
    coordinator.on("GET", "/api/v1/tables", () =>
        json(200, schema("TablePage", { items: [], next_cursor: null })),
    );
    await page.goto("/tables");
    await expect(page.getByText("No tables are visible to you.")).toBeVisible();

    const sinkThrew = await page.evaluate(() => {
        const style = document.createElement("style");
        style.textContent = "body { color: red }";
        document.head.append(style);
        try {
            document.body.insertAdjacentHTML("beforeend", "<b>raw</b>");
            return false;
        } catch {
            return true;
        }
    });

    expect(sinkThrew).toBe(true);
    await expect
        .poll(() => problems.some((problem) => problem.includes("style-src")))
        .toBe(true);
    // The violations were the point of this test.
    problems.length = 0;
});
