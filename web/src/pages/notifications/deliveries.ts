// Presentation shared by the delivery log and one delivery's detail: the
// event-kind vocabulary already defined for rules, and the badge tone each
// state or attempt outcome earns. The text always carries the meaning.

import type { Schemas } from "../../api/client";
import { words } from "../../format";
import { EVENT_KINDS } from "./rules";

/** An event kind such as `job.failed` as words; unknown kinds read plainly. */
export function deliveryEventLabel(kind: string): string {
    return (
        EVENT_KINDS.find((option) => option.value === kind)?.label ??
        words(kind.replaceAll(".", " "))
    );
}

/** The badge tone for a delivery state. */
export function deliveryStateTone(
    state: Schemas["NotificationDelivery"]["state"],
): "info" | "success" | "destructive" {
    switch (state) {
        case "pending":
            return "info";
        case "delivered":
            return "success";
        case "exhausted":
            return "destructive";
    }
}

/** The badge tone for an attempt outcome. */
export function attemptOutcomeTone(
    outcome: Schemas["NotificationAttempt"]["outcome"],
): "success" | "warning" | "destructive" {
    switch (outcome) {
        case "delivered":
            return "success";
        case "rejected":
            return "warning";
        case "failed":
            return "destructive";
    }
}
