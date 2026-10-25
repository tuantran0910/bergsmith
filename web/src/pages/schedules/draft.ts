// A schedule's editable fields as the form holds them, and the requests those
// fields build. Times are wall-clock in the schedule's time zone; the API takes
// minutes after midnight, so the draft converts at the boundary.

import type { Schemas } from "../../api/client";

/** One schedule's fields while they are being edited. */
export interface ScheduleDraft {
    policyId: string;
    tableId: string;
    serviceId: string;
    cron: string;
    zone: string;
    windowed: boolean;
    /** Start of the daily window as `HH:MM`, inclusive. */
    start: string;
    /** End of the daily window as `HH:MM`, exclusive. */
    end: string;
    enabled: boolean;
}

/** A daily run, which an operator can then adjust. */
export const DEFAULT_CRON = "0 2 * * *";

/** The browser's zone when it is a regional IANA name, else UTC. */
export function defaultZone(): string {
    const zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
    return typeof zone === "string" && zone.includes("/") ? zone : "UTC";
}

export function newDraft(zone = defaultZone()): ScheduleDraft {
    return {
        policyId: "",
        tableId: "",
        serviceId: "",
        cron: DEFAULT_CRON,
        zone,
        windowed: false,
        start: "02:00",
        end: "05:00",
        enabled: true,
    };
}

export function draftOf(
    schedule: Schemas["MaintenanceSchedule"],
): ScheduleDraft {
    const window = schedule.window;
    return {
        policyId: schedule.policy_id,
        tableId: schedule.table_id,
        serviceId: schedule.service_principal_id,
        cron: schedule.cron_expression,
        zone: schedule.time_zone,
        windowed: window !== null,
        start: window === null ? "02:00" : clock(window.start_minute),
        end: window === null ? "05:00" : clock(window.end_minute),
        enabled: schedule.enabled,
    };
}

/** Minutes after midnight from `HH:MM`, or `null` when it is not a time. */
export function minutes(value: string): number | null {
    const found = /^(\d{2}):(\d{2})$/.exec(value);
    if (found === null) {
        return null;
    }
    const hour = Number(found[1]);
    const minute = Number(found[2]);
    if (hour > 23 || minute > 59) {
        return null;
    }
    return hour * 60 + minute;
}

/** `HH:MM` for minutes after midnight. */
export function clock(value: number): string {
    const hour = Math.floor(value / 60);
    return `${String(hour).padStart(2, "0")}:${String(value % 60).padStart(2, "0")}`;
}

/** The window the draft names, or `null` when it names none or is invalid. */
function windowOf(
    draft: ScheduleDraft,
): Schemas["MaintenanceWindow"] | null | undefined {
    if (!draft.windowed) {
        return null;
    }
    const start = minutes(draft.start);
    const end = minutes(draft.end);
    // Equal bounds name no window at all, which the coordinator rejects; the
    // form refuses to submit that rather than guess an intent.
    if (start === null || end === null || start === end) {
        return undefined;
    }
    return { start_minute: start, end_minute: end };
}

/**
 * The create request the draft describes, or `null` while a field is missing.
 * The coordinator checks cron syntax, the zone name, and the window bounds.
 */
export function createBody(
    draft: ScheduleDraft,
): Schemas["MaintenanceScheduleCreate"] | null {
    const window = windowOf(draft);
    const cron = draft.cron.trim();
    const zone = draft.zone.trim();
    if (
        window === undefined ||
        draft.policyId === "" ||
        draft.tableId === "" ||
        draft.serviceId === "" ||
        cron === "" ||
        zone === ""
    ) {
        return null;
    }
    return {
        policy_id: draft.policyId,
        table_id: draft.tableId,
        service_principal_id: draft.serviceId,
        cron_expression: cron,
        time_zone: zone,
        window,
    };
}

/** Whether two windows describe the same daily range. */
function sameWindow(
    left: Schemas["MaintenanceWindow"] | null | undefined,
    right: Schemas["MaintenanceWindow"] | null | undefined,
): boolean {
    if (left === null || left === undefined) {
        return right === null || right === undefined;
    }
    return (
        right !== null &&
        right !== undefined &&
        left.start_minute === right.start_minute &&
        left.end_minute === right.end_minute
    );
}

/**
 * The update request for `schedule`, naming the revision the form read. Only
 * changed fields are sent; the policy, table, and service identity cannot
 * change. `null` means the draft is incomplete.
 */
export function updateBody(
    schedule: Schemas["MaintenanceSchedule"],
    draft: ScheduleDraft,
): Schemas["MaintenanceScheduleUpdate"] | null {
    const next = createBody(draft);
    const window = windowOf(draft);
    if (next === null || window === undefined) {
        return null;
    }
    const changes: Schemas["MaintenanceScheduleUpdate"] = {
        revision: schedule.revision,
    };
    if (next.cron_expression !== schedule.cron_expression) {
        changes.cron_expression = next.cron_expression;
    }
    if (next.time_zone !== schedule.time_zone) {
        changes.time_zone = next.time_zone;
    }
    if (!sameWindow(window, schedule.window)) {
        changes.window = window;
    }
    if (draft.enabled !== schedule.enabled) {
        changes.enabled = draft.enabled;
    }
    return changes;
}

/** Whether the draft names any change at all. */
export function changed(
    schedule: Schemas["MaintenanceSchedule"],
    draft: ScheduleDraft,
): boolean {
    const body = updateBody(schedule, draft);
    return (
        body !== null && Object.keys(body).some((field) => field !== "revision")
    );
}
