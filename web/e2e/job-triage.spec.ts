// Job triage in a real browser (BS-032.T5): a failed job can be traced to its
// task and attempt, with the failure reason, duration, worker, and outputs,
// without leaving the page.

import {
    JOB_ID,
    catalogs,
    event,
    job,
    jobTasks,
    session,
} from "../src/test/fixtures";
import {
    type FakeCoordinator,
    eventStream,
    expect,
    expectAccessible,
    json,
    test,
} from "./coordinator";

function signedIn(coordinator: FakeCoordinator) {
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Operator")))
        .on("GET", "/api/v1/jobs/{id}", () =>
            json(
                200,
                job("Failed", {
                    state_reason: "ProcessMemoryLimitExceeded",
                }),
            ),
        )
        .on("GET", "/api/v1/jobs/{id}/tasks", () => json(200, jobTasks))
        .on("GET", "/api/v1/jobs/{id}/events", () =>
            eventStream([event(1, "Failed", null)]),
        )
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs));
}

test("traces a failed job to its task and attempt", async ({
    page,
    coordinator,
}) => {
    signedIn(coordinator);
    await page.goto(`/jobs/${JOB_ID}`);

    await page.getByRole("tab", { name: "Tasks" }).click();
    await expect(page).toHaveURL(`/jobs/${JOB_ID}?tab=tasks`);
    const tasks = page.getByRole("table", { name: "Job tasks" });
    await expect(tasks.getByText("Task 1")).toBeVisible();
    await expect(tasks.getByText(/ProcessMemoryLimitExceeded/)).toBeVisible();
    await expect(tasks.getByText(/PermanentFailure/)).toBeVisible();
    await expect(tasks.getByText("4 s", { exact: true })).toBeVisible();
    await expect(tasks.getByText(/default\/worker-1/)).toBeVisible();
    await expect(tasks.getByText("0 files · 0 B")).toBeVisible();
    await expectAccessible(page);
});

test("links an attention state to the tasks tab", async ({
    page,
    coordinator,
}) => {
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Operator")))
        .on("GET", "/api/v1/jobs/{id}", () =>
            json(200, job("CommitOutcomeUnknown")),
        )
        .on("GET", "/api/v1/jobs/{id}/tasks", () => json(200, jobTasks))
        .on("GET", "/api/v1/jobs/{id}/events", () =>
            eventStream([event(1, "CommitOutcomeUnknown", null)]),
        )
        .on("GET", "/api/v1/catalogs", () => json(200, catalogs));
    await page.goto(`/jobs/${JOB_ID}`);

    await expect(
        page.getByRole("button", { name: "Inspect tasks" }),
    ).toBeVisible();
    await page.getByRole("button", { name: "Inspect tasks" }).click();
    await expect(page.getByRole("tab", { name: "Tasks" })).toHaveAttribute(
        "aria-selected",
        "true",
    );
    await expect(
        page.getByRole("table", { name: "Job tasks" }).getByText("Task 1"),
    ).toBeVisible();
    await expectAccessible(page);
});
