import { tableStorage } from "../src/test/fixtures";
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
    tableHealth,
    overview,
    tableMaintenance,
    tableRecommendations,
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

test("submits orphan cleanup from the table page", async ({
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
        .on("POST", "/api/v1/jobs", () =>
            json(202, {
                operation_id: JOB_ID,
                job_id: JOB_ID,
                resource_url: `/api/v1/jobs/${JOB_ID}`,
            }),
        )
        .on("GET", "/api/v1/jobs/{id}", () =>
            json(200, job("Deleting", { operation: "remove_orphan_files" })),
        )
        .on("GET", "/api/v1/jobs/{id}/events", () =>
            eventStream([event(1, "Planning", null)]),
        );

    await page.goto("/tables");
    await page.getByRole("link", { name: /^orders/ }).click();
    await expect(
        page.getByRole("heading", { name: "lake.sales.orders" }),
    ).toBeVisible();
    await page.getByRole("button", { name: "More operations" }).click();
    await page.getByRole("menuitem", { name: "Remove orphan files" }).click();
    await page
        .getByRole("button", { name: "Confirm remove orphan files" })
        .click();
    await expect(page).toHaveURL(`/jobs/${JOB_ID}`);
    await expect(
        page.getByText("Not applicable", { exact: true }),
    ).toBeVisible();
    await expectAccessible(page);

    expect(coordinator.sent("POST", "/api/v1/jobs")[0]?.body).toEqual({
        table_id: TABLE_ID,
        operation: "remove_orphan_files",
    });
});

test("submits a manifest rewrite from the table page", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator
        .on("POST", "/api/v1/jobs", () =>
            json(202, {
                operation_id: JOB_ID,
                job_id: JOB_ID,
                resource_url: `/api/v1/jobs/${JOB_ID}`,
            }),
        )
        .on("GET", "/api/v1/jobs/{id}", () =>
            json(200, job("ReadyToCommit", { operation: "rewrite_manifests" })),
        )
        .on("GET", "/api/v1/jobs/{id}/events", () =>
            eventStream([event(1, "Planning", null)]),
        );

    await page.goto(`/tables/${TABLE_ID}`);
    const more = page.getByRole("button", { name: "More operations" });
    await more.click();
    await page.getByRole("menuitem", { name: "Rewrite manifests" }).click();
    const dialog = page.getByRole("dialog", { name: "Rewrite manifests" });
    await expect(dialog).toContainText("No data or delete file changes");
    // Leaving the confirmation returns focus to the menu that opened it.
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
    await expect(more).toBeFocused();
    await more.click();
    await page.getByRole("menuitem", { name: "Rewrite manifests" }).click();
    await expect(dialog).toContainText("No data or delete file changes");
    await expectAccessible(page);
    await page
        .getByRole("button", { name: "Confirm rewrite manifests" })
        .click();
    await expect(page).toHaveURL(`/jobs/${JOB_ID}`);
    await expect(page.getByText("Manifest rewrite").first()).toBeVisible();
    await expect(page.getByText("Plan ready", { exact: true })).toBeVisible();
    await expectAccessible(page);

    expect(coordinator.sent("POST", "/api/v1/jobs")[0]?.body).toEqual({
        table_id: TABLE_ID,
        operation: "rewrite_manifests",
    });
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
    await page.getByRole("tab", { name: "Statistics" }).click();
    await expect(
        page.getByRole("heading", { name: "Maintenance statistics" }),
    ).toBeVisible();
    await expect(
        page.getByText("Input files rewritten").locator(".."),
    ).toContainText("8");
    await expect(
        page.getByText(/Accepted outputs were committed to the table/),
    ).toBeVisible();
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

test("searches and filters tables on the coordinator, kept in the address", async ({
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
    coordinator
        .on("GET", "/api/v1/tables", (sent) =>
            json(
                200,
                schema("TablePage", {
                    items:
                        new URLSearchParams(sent.search).get("q") === "events"
                            ? [other]
                            : [table, other],
                    next_cursor: null,
                }),
            ),
        )
        .on("GET", "/api/v1/tables/facets", () =>
            json(
                200,
                schema("TableFacets", {
                    health: [{ value: "unassessed", count: 2 }],
                    catalog: [{ value: CATALOG_ID, count: 2 }],
                }),
            ),
        );

    await page.goto("/tables");
    await expect(page.getByRole("link", { name: /^orders/ })).toBeVisible();
    await page.getByRole("searchbox", { name: "Search tables" }).fill("events");

    await expect(page.getByRole("link", { name: /^orders/ })).toBeHidden();
    await expect(page.getByRole("link", { name: /^events/ })).toBeVisible();
    await expect(page).toHaveURL(/q=events/);
    // The coordinator searched once for the whole text.
    expect(
        coordinator.requests
            .filter((sent) => sent.pathname === "/api/v1/tables")
            .map((sent) => new URLSearchParams(sent.search).get("q"))
            .filter((q) => q !== null),
    ).toEqual(["events"]);
    await expectAccessible(page);

    // A facet opens in a popover with its counts; the choice becomes a pill.
    await page.getByRole("button", { name: "Health filter" }).click();
    const notAssessed = page.getByRole("radio", { name: "Not assessed 2" });
    await expect(notAssessed).toBeVisible();
    await expectAccessible(page);
    await notAssessed.check();
    await page.keyboard.press("Escape");
    await expect(page).toHaveURL(/health=unassessed/);
    await expect(
        page.getByRole("button", {
            name: "Remove filter Health: Not assessed",
        }),
    ).toBeVisible();

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

    // On wide screens the density lives in the account panel.
    const account = page
        .getByRole("region", { name: "Account" })
        .getByRole("button", { name: "Alice" });
    await account.click();
    await page.getByRole("button", { name: "Compact density" }).click();
    await expect(page.locator("html")).toHaveAttribute(
        "data-density",
        "compact",
    );
    await page.keyboard.press("Escape");
    await expect(account).toBeFocused();
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
    await account.click();
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

for (const width of [320, 390]) {
    test(`fits detail headers into ${width} px`, async ({
        page,
        coordinator,
    }, testInfo) => {
        signedIn(coordinator, "Administrator");
        coordinator
            .on("GET", "/api/v1/jobs/{id}", () => json(200, job("Running")))
            .on("GET", "/api/v1/jobs/{id}/events", () =>
                eventStream([event(1, "Planning", null)]),
            );
        await page.setViewportSize({ width, height: 844 });
        for (const [path, title] of [
            [`/jobs/${JOB_ID}`, "lake.sales.orders"],
            [`/tables/${TABLE_ID}`, "lake.sales.orders"],
        ] as const) {
            await page.goto(path);
            await expect(
                page.getByRole("heading", { level: 1, name: title }),
            ).toBeVisible();
            const overflow = await page.evaluate(
                () =>
                    document.documentElement.scrollWidth -
                    document.documentElement.clientWidth,
            );
            expect(overflow).toBeLessThanOrEqual(0);
            await page.screenshot({
                path: testInfo.outputPath(
                    `${path.split("/")[1] ?? "page"}-${width}.png`,
                ),
                fullPage: true,
            });
            await expectAccessible(page);
        }
    });
}

test("keeps a detail page's header in reach and focus clear of it", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator, "Administrator");
    await page.setViewportSize({ width: 1440, height: 320 });
    await page.goto(`/tables/${TABLE_ID}`);
    const title = page.getByRole("heading", {
        level: 1,
        name: "lake.sales.orders",
    });
    await expect(title).toBeVisible();
    await page.evaluate(() => {
        window.scrollTo(0, document.documentElement.scrollHeight);
    });
    expect(await page.evaluate(() => window.scrollY)).toBeGreaterThan(0);
    // Scrolled to the end, the header sits right under the 48 px top bar
    // rather than where it started, 24 px further down.
    await expect
        .poll(async () => (await title.boundingBox())?.y ?? -1)
        .toBeGreaterThanOrEqual(48);
    expect((await title.boundingBox())?.y).toBeLessThan(60);
    await expect(
        page.getByRole("button", { name: "Run maintenance" }),
    ).toBeInViewport();
    const padding = await page.evaluate(() => {
        const root = document.documentElement;
        const header = document.querySelector<HTMLElement>(
            '[data-slot="detail-header"]',
        );
        return {
            padding: Number.parseFloat(getComputedStyle(root).scrollPaddingTop),
            expected: 48 + (header?.offsetHeight ?? 0),
        };
    });
    expect(padding.padding).toBe(padding.expected);
});

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
            // The probe: this sink must throw under the policy.
            // eslint-disable-next-line no-restricted-syntax
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

test("filters tables by health and opens the health tab", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator
        .on("GET", "/api/v1/tables", (request) =>
            json(
                200,
                schema("TablePage", {
                    items:
                        new URLSearchParams(request.search).get("health") ===
                        "critical"
                            ? [
                                  schema("Table", {
                                      ...table,
                                      health: tableHealth("critical"),
                                  }),
                              ]
                            : [
                                  schema("Table", {
                                      ...table,
                                      health: tableHealth("warning"),
                                  }),
                              ],
                    next_cursor: null,
                }),
            ),
        )
        .on("GET", "/api/v1/tables/{id}", () =>
            json(
                200,
                schema("Table", {
                    ...table,
                    health: tableHealth("critical"),
                }),
            ),
        )
        .on("GET", "/api/v1/tables/{id}/recommendations", () =>
            json(200, tableRecommendations()),
        )
        .on("GET", "/api/v1/tables/facets", () =>
            json(
                200,
                schema("TableFacets", {
                    health: [
                        { value: "critical", count: 1 },
                        { value: "warning", count: 1 },
                    ],
                    catalog: [],
                }),
            ),
        );

    await page.goto("/tables");
    await expect(page.getByRole("cell", { name: "Warning" })).toBeVisible();
    await page.getByRole("button", { name: "Health filter" }).click();
    await page.getByRole("radio", { name: "Critical 1" }).check();
    await page.keyboard.press("Escape");
    await expect(page).toHaveURL(/health=critical/);
    await expect(page.getByRole("cell", { name: "Critical" })).toBeVisible();
    await expectAccessible(page);

    await page.goto(`/tables/${TABLE_ID}?tab=health`);
    await expect(page.getByRole("cell", { name: "Small files" })).toBeVisible();
    // Every finding names the one threshold set the assessment used.
    await expect(
        page.getByRole("cell", { name: "Policy “nightly”" }),
    ).toHaveCount(2);
    await expect(page.getByText(/5 files \(20.0 MiB\)/)).toBeVisible();
    await expectAccessible(page);
});

test("shows sampled storage with candidate and scope explanations", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    coordinator.on("GET", "/api/v1/tables/{id}/storage", () =>
        json(200, tableStorage),
    );
    await page.setViewportSize({ width: 320, height: 800 });
    await page.goto(`/tables/${TABLE_ID}?tab=storage`);
    await expect(
        page.getByRole("region", { name: "Storage breakdown" }),
    ).toBeVisible();
    await expect(
        page.getByRole("rowheader", { name: "Orphan candidates" }),
    ).toBeVisible();
    await expect(
        page.getByText(/not a promise of reclaimable space/),
    ).toBeVisible();
    await expectAccessible(page);
    await page.setViewportSize({ width: 1440, height: 900 });
    await expectAccessible(page);
});

test("filters jobs by state and hides a column", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    const running = job("Running", {
        id: "0190f3c4-0000-7000-8000-00000000aaaa",
    });
    const failed = job("Failed", {
        id: "0190f3c4-0000-7000-8000-00000000bbbb",
    });
    coordinator
        .on("GET", "/api/v1/jobs", (sent) =>
            json(
                200,
                schema("JobPage", {
                    items:
                        new URLSearchParams(sent.search).get("state") ===
                        "Failed"
                            ? [failed]
                            : [running, failed],
                    next_cursor: null,
                }),
            ),
        )
        .on("GET", "/api/v1/jobs/facets", () =>
            json(
                200,
                schema("JobFacets", {
                    state: [
                        { value: "Running", count: 1 },
                        { value: "Failed", count: 1 },
                    ],
                    operation: [{ value: "compact", count: 2 }],
                }),
            ),
        );

    await page.goto("/jobs");
    await expect(
        page.getByRole("link", { name: "Job 0000aaaa" }),
    ).toBeVisible();
    await page.getByRole("button", { name: "State filter" }).click();
    await page.getByRole("checkbox", { name: "Failed 1" }).check();
    await page.keyboard.press("Escape");
    await expect(page).toHaveURL(/state=Failed/);
    await expect(page.getByRole("link", { name: "Job 0000aaaa" })).toBeHidden();
    await expect(
        page.getByRole("link", { name: "Job 0000bbbb" }),
    ).toBeVisible();
    await expectAccessible(page);

    await page.getByRole("button", { name: "Columns" }).click();
    await page.getByRole("menuitemcheckbox", { name: "Requested by" }).click();
    await page.keyboard.press("Escape");
    await expect(
        page.getByRole("columnheader", { name: "Requested by" }),
    ).toBeHidden();
    // The choice is this browser's, and survives a reload.
    await page.reload();
    await expect(
        page.getByRole("columnheader", { name: "State" }),
    ).toBeVisible();
    await expect(
        page.getByRole("columnheader", { name: "Requested by" }),
    ).toBeHidden();
});

test("keeps a thousand jobs responsive by rendering only rows in view", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    const jobs = Array.from({ length: 1000 }, (_value, index) =>
        job("Running", {
            id: `0190f3c4-0000-7000-8000-${String(index).padStart(12, "0")}`,
        }),
    );
    let served = 0;
    coordinator.on("GET", "/api/v1/jobs", () => {
        const items = jobs.slice(served, served + 50);
        served += 50;
        return json(
            200,
            schema("JobPage", {
                items,
                next_cursor: served < jobs.length ? `page-${served}` : null,
            }),
        );
    });

    await page.goto("/jobs");
    const more = page.getByRole("button", { name: "Load more jobs" });
    // Twenty pages of fifty; each press must stay responsive.
    for (let pageNumber = 1; pageNumber < 20; pageNumber++) {
        await more.click();
        await expect(
            page.getByText(new RegExp(`^${(pageNumber + 1) * 50} shown`)),
        ).toBeVisible();
    }
    const grid = page.getByRole("table", { name: "Jobs" });
    await expect(grid).toHaveAttribute("aria-rowcount", "1001");
    const rendered = await grid.locator("tbody tr[data-index]").count();
    expect(rendered).toBeLessThan(100);

    // Interaction to next paint and layout shift, at 4× CPU throttle: hiding
    // a column re-renders every row the grid keeps.
    await page.evaluate(() => {
        const vitals = { inp: 0, cls: 0 };
        (window as unknown as { __vitals: typeof vitals }).__vitals = vitals;
        new PerformanceObserver((list) => {
            for (const entry of list.getEntries()) {
                if (
                    (entry as PerformanceEntry & { interactionId?: number })
                        .interactionId
                ) {
                    vitals.inp = Math.max(vitals.inp, entry.duration);
                }
            }
        }).observe({
            type: "event",
            buffered: true,
            // Not yet in TypeScript's DOM types.
            durationThreshold: 16,
        } as PerformanceObserverInit);
        new PerformanceObserver((list) => {
            for (const entry of list.getEntries()) {
                const shift = entry as PerformanceEntry & {
                    value: number;
                    hadRecentInput: boolean;
                };
                if (!shift.hadRecentInput) {
                    vitals.cls += shift.value;
                }
            }
        }).observe({ type: "layout-shift", buffered: true });
    });
    const cdp = await page.context().newCDPSession(page);
    await cdp.send("Emulation.setCPUThrottlingRate", { rate: 4 });
    await page.getByRole("button", { name: "Columns" }).click();
    await page.getByRole("menuitemcheckbox", { name: "Operation" }).click();
    await page.keyboard.press("Escape");
    await expect(
        page.getByRole("columnheader", { name: "Operation" }),
    ).toBeHidden();
    await cdp.send("Emulation.setCPUThrottlingRate", { rate: 1 });
    const vitals = await page.evaluate(
        () =>
            (window as unknown as { __vitals: { inp: number; cls: number } })
                .__vitals,
    );
    expect(vitals.inp, "interaction to next paint (ms)").toBeLessThanOrEqual(
        200,
    );
    expect(vitals.cls, "cumulative layout shift").toBeLessThanOrEqual(0.1);

    // Arrow keys walk the rows, scrolling the list as they go.
    await grid.locator("tbody tr[data-index] a").first().focus();
    await page.keyboard.press("End");
    await expect(
        grid.locator('tbody tr[data-index="999"] a').first(),
    ).toBeFocused();
    await page.keyboard.press("ArrowUp");
    await expect(
        grid.locator('tbody tr[data-index="998"] a').first(),
    ).toBeFocused();
    await expectAccessible(page);
});

test("previews a table and saves the list as a view", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    const saved = schema("SavedView", {
        id: "0190f3c4-0000-7000-8000-00000000cafe",
        page: "tables",
        name: "Critical tables",
        params: { health: "critical" },
        revision: 1,
        created_at: "2026-10-30T00:00:00Z",
        updated_at: "2026-10-30T00:00:00Z",
    });
    let views: (typeof saved)[] = [];
    coordinator
        .on("GET", "/api/v1/tables", () =>
            json(
                200,
                schema("TablePage", { items: [table], next_cursor: null }),
            ),
        )
        .on("GET", "/api/v1/tables/{id}", () => json(200, table))
        .on("GET", "/api/v1/me/views", () => json(200, { items: views }))
        .on("POST", "/api/v1/me/views", () => {
            views = [saved];
            return json(201, saved);
        });

    await page.goto("/tables");
    const preview = page.getByRole("button", { name: /^Preview table orders/ });
    await preview.click();
    const panel = page.getByRole("dialog", { name: "orders" });
    await expect(panel.getByRole("link", { name: "Open table" })).toBeVisible();
    await expect(page).toHaveURL(new RegExp(`peek=${TABLE_ID}`));
    await expect(panel).toHaveCSS("opacity", "1");
    await expectAccessible(page);
    await page.keyboard.press("Escape");
    await expect(panel).toBeHidden();
    await expect(page).not.toHaveURL(/peek=/);
    await expect(preview).toBeFocused();

    await page.goto("/tables?health=critical");
    await page.getByRole("button", { name: "Views" }).click();
    await page.getByRole("menuitem", { name: "Save current view…" }).click();
    const dialog = page.getByRole("dialog", { name: "Save view" });
    await dialog.getByLabel("Name").fill("Critical tables");
    await expect(dialog).toHaveCSS("opacity", "1");
    await expectAccessible(page);
    await dialog.getByRole("button", { name: "Save view" }).click();
    await expect(dialog).toBeHidden();
    await expect(
        page.getByRole("button", { name: "Critical tables view" }),
    ).toBeVisible();
    const post = coordinator.requests.find(
        (sent) =>
            sent.method === "POST" && sent.pathname === "/api/v1/me/views",
    );
    expect(post?.headers["idempotency-key"]).toBeTruthy();
    expect(post?.body).toEqual({
        page: "tables",
        name: "Critical tables",
        params: { health: "critical" },
    });
});
