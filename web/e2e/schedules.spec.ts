// Creating a schedule and reading its next instants in a real browser, under
// the coordinator's CSP, with axe checking the list, the dialog, and the detail
// (BS-024.T2b, the acceptance criterion of BS-024).

import type { Schemas } from "../src/api/client";
import { schema } from "../src/test/fakes";
import {
    POLICY_ID,
    SCHEDULE_ID,
    SERVICE_ID,
    TABLE_ID,
    catalogs,
    policies,
    schedule,
    scheduleOccurrences,
    serviceIdentity,
    session,
    table,
} from "../src/test/fixtures";
import { expect, expectAccessible, json, test } from "./coordinator";

test("creates a schedule and shows its next instants", async ({
    page,
    coordinator,
}) => {
    let stored: Schemas["MaintenanceSchedule"][] = [];
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
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
        )
        .on("GET", "/api/v1/schedules", () =>
            json(
                200,
                schema("MaintenanceSchedulePage", {
                    items: stored,
                    next_cursor: null,
                }),
            ),
        )
        .on("POST", "/api/v1/schedules", (sent) => {
            const body = sent.body as Schemas["MaintenanceScheduleCreate"];
            const created = schedule({
                id: SCHEDULE_ID,
                policy_id: body.policy_id,
                table_id: body.table_id,
                service_principal_id: body.service_principal_id,
                cron_expression: body.cron_expression,
                time_zone: body.time_zone,
                window: body.window ?? null,
            });
            stored = [created];
            return json(201, created);
        })
        .on("GET", "/api/v1/schedules/{id}", () =>
            json(200, stored[0] ?? schedule()),
        )
        .on("GET", "/api/v1/schedules/{id}/occurrences", () =>
            json(200, scheduleOccurrences),
        );

    await page.goto("/schedules");
    await expect(
        page.getByRole("heading", { name: "Schedules" }),
    ).toBeVisible();
    await expect(page.getByText("No schedules yet.")).toBeVisible();
    await expectAccessible(page);

    await page.getByRole("button", { name: "New schedule" }).click();
    await expect(
        page.getByRole("heading", { name: "New schedule" }),
    ).toBeVisible();
    await page.getByRole("combobox", { name: "Policy" }).click();
    await page.getByRole("option", { name: "nightly-compact" }).click();
    await page.getByRole("combobox", { name: "Service identity" }).click();
    await page.getByRole("option", { name: "scheduler" }).click();
    await page.getByRole("combobox", { name: "Table" }).click();
    await page.getByRole("option", { name: "lake.sales.orders" }).click();
    await expectAccessible(page);
    await page.getByRole("button", { name: "Create schedule" }).click();

    const created = coordinator.sent("POST", "/api/v1/schedules")[0];
    expect(created?.body).toMatchObject({
        policy_id: POLICY_ID,
        table_id: TABLE_ID,
        service_principal_id: SERVICE_ID,
        cron_expression: "0 2 * * *",
        window: null,
    });
    expect(created?.headers["x-csrf-token"]).toBe("csrf-value");

    // The refreshed list leads with the table the schedule maintains.
    await page
        .getByRole("link", {
            name: "Open the schedule for lake.sales.orders",
        })
        .click();
    await expect(page).toHaveURL(`/schedules/${SCHEDULE_ID}`);
    await expect(
        page.getByRole("heading", { name: "lake.sales.orders" }),
    ).toBeVisible();
    await expect(
        page.getByRole("list", { name: "Next instants" }).getByRole("listitem"),
    ).toHaveCount(2);
    await expect(
        page.getByRole("table", { name: "Schedule evaluations" }),
    ).toBeVisible();
    await expect(page.getByText("Outside window")).toBeVisible();
    await expectAccessible(page);
});
