// Notification administration and the delivery log in a real browser, under
// the coordinator's CSP, with axe checking each view (BS-029.T4).

import type { Schemas } from "../src/api/client";
import { schema } from "../src/test/fakes";
import {
    DELIVERY_ID,
    notificationDelivery,
    notificationDeliveryDetail,
    notificationDestination,
    notificationRule,
    session,
} from "../src/test/fixtures";
import { expect, expectAccessible, json, test } from "./coordinator";

const CREATED_ID = "0190f3c4-0000-7000-8000-0000000000f9";

function destinationPage(items: readonly Schemas["NotificationDestination"][]) {
    return json(
        200,
        schema("NotificationDestinationPage", {
            items: [...items],
            next_cursor: null,
        }),
    );
}

function rulePage(items: readonly Schemas["NotificationRule"][]) {
    return json(
        200,
        schema("NotificationRulePage", {
            items: [...items],
            next_cursor: null,
        }),
    );
}

test("manages destinations and rules, then reads a delivery", async ({
    page,
    coordinator,
}) => {
    // One journey covers four pages and an axe pass on each; give its teardown
    // room on a loaded machine.
    test.setTimeout(60_000);
    let destinations: Schemas["NotificationDestination"][] = [
        notificationDestination(),
    ];
    const rules = [notificationRule()];
    const delivery = notificationDelivery();
    coordinator
        .on("GET", "/auth/session", () => json(200, session("Administrator")))
        .on("GET", "/api/v1/notifications/destinations", () =>
            destinationPage(destinations),
        )
        .on("POST", "/api/v1/notifications/destinations", (sent) => {
            const body = sent.body as Schemas["CreateNotificationDestination"];
            expect(body).toMatchObject({
                name: "release hooks",
                kind: "webhook",
                url_ref: "release-url",
                signing_secret_ref: "release-key",
            });
            expect(sent.headers["x-csrf-token"]).toBe("csrf-value");
            const created = notificationDestination({
                id: CREATED_ID,
                name: body.name,
                url_ref: body.url_ref,
                signing_secret_ref: body.signing_secret_ref ?? null,
                revision: 1,
            });
            destinations = [...destinations, created];
            return json(201, created);
        })
        .on(
            "POST",
            "/api/v1/notifications/destinations/{id}/test",
            (_sent, params) => {
                expect(params.id).toBe(CREATED_ID);
                return json(200, {
                    delivered: true,
                    status: 204,
                    response: null,
                });
            },
        )
        .on("GET", "/api/v1/notifications/rules", () => rulePage(rules))
        .on("GET", "/api/v1/notifications/deliveries", () =>
            json(
                200,
                schema("NotificationDeliveryPage", {
                    items: [delivery],
                    next_cursor: null,
                }),
            ),
        )
        .on("GET", "/api/v1/notifications/deliveries/{id}", (_sent, params) => {
            expect(params.id).toBe(DELIVERY_ID);
            return json(200, notificationDeliveryDetail());
        });

    await page.goto("/notifications");
    await expect(
        page.getByRole("heading", { name: "Notifications" }),
    ).toBeVisible();
    await expect(
        page.getByRole("link", { name: "Notifications" }).first(),
    ).toHaveAttribute("aria-current", "page");
    await expect(
        page.getByRole("table", { name: "Notification destinations" }),
    ).toBeVisible();
    await expect(page.getByRole("cell", { name: "ops-url" })).toBeVisible();
    await expectAccessible(page);

    await page.getByRole("button", { name: "New destination" }).click();
    await expect(
        page.getByRole("heading", { name: "New destination" }),
    ).toBeVisible();
    await page.getByLabel("Destination name").fill("release hooks");
    await page.getByLabel("URL reference").fill("release-url");
    await page.getByLabel("Signing secret reference").fill("release-key");
    await page.getByRole("button", { name: "Create destination" }).click();
    await expect(
        page.getByRole("row", { name: /release hooks/ }),
    ).toBeVisible();

    await page.getByRole("button", { name: "Test release hooks" }).click();
    await expect(page.getByText("Test delivered (204).")).toBeVisible();
    await expectAccessible(page);

    await page.getByRole("link", { name: "Rules" }).click();
    await expect(
        page.getByRole("heading", { name: "Notification rules" }),
    ).toBeVisible();
    await expect(page.getByRole("cell", { name: "Job failed" })).toBeVisible();
    await expectAccessible(page);

    await page.getByRole("link", { name: "Deliveries" }).click();
    await expect(
        page.getByRole("heading", { name: "Deliveries" }),
    ).toBeVisible();
    await expect(
        page.getByRole("table", { name: "Notification deliveries" }),
    ).toBeVisible();
    await page
        .getByRole("link", { name: `Open delivery ${DELIVERY_ID.slice(-8)}` })
        .click();
    await expect(
        page.getByRole("heading", { name: delivery.summary }),
    ).toBeVisible();
    await expect(
        page.getByRole("table", { name: "Delivery attempts" }),
    ).toBeVisible();
    await expect(
        page.getByText("Destination unreachable").first(),
    ).toBeVisible();
    await expectAccessible(page);
});
