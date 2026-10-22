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
        .on("GET", "/api/v1/tables/{id}", () => json(200, table));
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
    await page.getByRole("link", { name: "lake.sales.orders" }).click();
    await expect(
        page.getByRole("heading", { name: "lake.sales.orders" }),
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
            () => json(200, job("Succeeded", { publication_started: true })),
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

    await expect(page.getByText("Confirmed")).toBeVisible();
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
    await expect(page.getByRole("link", { name: JOB_ID })).toHaveAttribute(
        "href",
        `/jobs/${JOB_ID}`,
    );
    const retry = coordinator.sent("POST", `/api/v1/jobs/${JOB_ID}/retry`);
    expect(retry).toHaveLength(1);
    expect(retry[0]?.headers["idempotency-key"]).toMatch(/^[0-9a-f-]{36}$/);
    expect(
        coordinator.sent("GET", `/api/v1/jobs/${RETRY_JOB_ID}/events`),
    ).toHaveLength(1);
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
    await expect(page.getByRole("link", { name: "Audit" })).toHaveAttribute(
        "aria-current",
        "page",
    );
    await expectAccessible(page);

    await page.getByRole("link", { name: "Workers" }).click();
    await expect(page.getByText("worker-1")).toBeVisible();
    await expectAccessible(page);
});

test("stays accessible in the narrow layout", async ({ page, coordinator }) => {
    signedIn(coordinator);
    coordinator.on("GET", "/api/v1/tables", () =>
        json(200, schema("TablePage", { items: [table], next_cursor: null })),
    );
    await page.setViewportSize({ width: 390, height: 844 });

    await page.goto("/tables");
    await expect(
        page.getByRole("link", { name: "lake.sales.orders" }),
    ).toBeVisible();
    await expectAccessible(page);
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
