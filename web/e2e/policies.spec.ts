// The policy pages in a real browser, under the coordinator's CSP, with axe
// checking the list, both dialogs, and the conflict state (BS-024.T1).

import type { Schemas } from "../src/api/client";
import { schema } from "../src/test/fakes";
import { POLICY_ID, policies, policy, session } from "../src/test/fixtures";
import { expect, expectAccessible, json, test } from "./coordinator";

const CREATED_ID = "0190f3c4-0000-7000-8000-0000000000b2";

/** Answers a page of policies from the mutable store the test edits. */
function pageOf(store: readonly Schemas["MaintenancePolicy"][]) {
    return json(
        200,
        schema("MaintenancePolicyPage", {
            items: [...store],
            next_cursor: null,
        }),
    );
}

test("creates and edits a maintenance policy with its revision", async ({
    page,
    coordinator,
}) => {
    let stored: Schemas["MaintenancePolicy"][] = [...policies.items];
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/policies", () => pageOf(stored))
        .on("POST", "/api/v1/policies", (sent) => {
            const body = sent.body as Schemas["MaintenancePolicyCreate"];
            const created = policy({
                id: CREATED_ID,
                name: body.name,
                revision: 1,
                // The form sends every field; the assertion below pins them.
                target_file_size_bytes:
                    body.target_file_size_bytes ?? 268_435_456,
                min_input_files: body.min_input_files ?? 5,
                max_input_bytes: body.max_input_bytes ?? 107_374_182_400,
            });
            stored = [...stored, created];
            return json(201, created);
        })
        .on("PATCH", "/api/v1/policies/{id}", (sent, params) => {
            const body = sent.body as Schemas["MaintenancePolicyUpdate"];
            const current = stored.find((item) => item.id === params.id);
            if (current === undefined) {
                return json(404, { error: "missing" });
            }
            const updated = policy({
                ...current,
                ...body,
                revision: current.revision + 1,
            });
            stored = stored.map((item) =>
                item.id === current.id ? updated : item,
            );
            return json(200, updated);
        });

    await page.goto("/policies");
    await expect(page.getByRole("heading", { name: "Policies" })).toBeVisible();
    await expect(page.getByRole("link", { name: "Policies" })).toHaveAttribute(
        "aria-current",
        "page",
    );
    await expect(
        page.getByRole("table", { name: "Maintenance policies" }),
    ).toBeVisible();
    await expect(page.getByRole("cell", { name: "256.0 MiB" })).toBeVisible();
    await expectAccessible(page);

    await page.getByRole("button", { name: "New policy" }).click();
    await expect(
        page.getByRole("heading", { name: "New policy" }),
    ).toBeVisible();
    await expect(page.getByLabel("Target file size (bytes)")).toHaveValue(
        "268435456",
    );
    await expect(page.getByLabel("Run budget (bytes)")).toHaveValue(
        "107374182400",
    );
    await page.getByLabel("Policy name").fill("weekly-compact");
    await page.getByLabel("Minimum input files").fill("8");
    await expectAccessible(page);
    await page.getByRole("button", { name: "Create policy" }).click();

    // The list is read again, so the new policy appears without a reload,
    // and the page says what was created.
    await expect(
        page.getByRole("button", { name: "Edit weekly-compact" }),
    ).toBeVisible();
    await expect(page.getByText("Created weekly-compact.")).toBeVisible();
    const created = coordinator.sent("POST", "/api/v1/policies")[0];
    expect(created?.body).toEqual({
        name: "weekly-compact",
        target_file_size_bytes: 268_435_456,
        min_input_files: 8,
        max_input_bytes: 107_374_182_400,
    });
    expect(created?.headers["x-csrf-token"]).toBe("csrf-value");

    await page.getByRole("button", { name: "Edit weekly-compact" }).click();
    await expect(
        page.getByRole("heading", { name: "Edit weekly-compact" }),
    ).toBeVisible();
    await page.getByLabel("Target file size (bytes)").fill("536870912");
    await expectAccessible(page);
    await page.getByRole("button", { name: "Save policy" }).click();
    await expect(
        page.getByRole("heading", { name: "Edit weekly-compact" }),
    ).toBeHidden();
    const edit = coordinator.sent("PATCH", `/api/v1/policies/${CREATED_ID}`)[0];
    expect(edit?.body).toEqual({
        revision: 1,
        target_file_size_bytes: 536_870_912,
    });
    await expect(page.getByRole("cell", { name: "512.0 MiB" })).toBeVisible();
    await expect(page.getByText("Saved weekly-compact.")).toBeVisible();
});

test("stops a save when the policy changed and shows the current revision", async ({
    page,
    coordinator,
}) => {
    let stored: Schemas["MaintenancePolicy"][] = [policy()];
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/policies", () => pageOf(stored))
        .on("PATCH", "/api/v1/policies/{id}", () => {
            // Another editor saved first, so the form's revision is stale.
            stored = [
                policy({ revision: 4, target_file_size_bytes: 1_073_741_824 }),
            ];
            return json(409, {
                reason: "revision_conflict",
                disposition: "PermanentFailure",
                operator_action: "reload_and_retry",
            });
        });

    await page.goto("/policies");
    await page.getByRole("button", { name: "Edit nightly-compact" }).click();
    await page.getByLabel("Target file size (bytes)").fill("536870912");
    await page.getByRole("button", { name: "Save policy" }).click();
    await expect(
        page.getByText(/changed while you were editing/),
    ).toBeVisible();
    // The rejected edits stay put, and no retry is sent with them.
    await expect(page.getByLabel("Target file size (bytes)")).toHaveValue(
        "536870912",
    );
    await expect(
        page.getByRole("button", { name: "Save policy" }),
    ).toBeDisabled();
    await expectAccessible(page);

    await page.getByRole("button", { name: "Reload revision 4" }).click();
    await expect(page.getByLabel("Target file size (bytes)")).toHaveValue(
        "1073741824",
    );
    await expect(
        page.getByRole("button", { name: "Save policy" }),
    ).toBeDisabled();
    expect(
        coordinator.sent("PATCH", `/api/v1/policies/${POLICY_ID}`),
    ).toHaveLength(1);
});
