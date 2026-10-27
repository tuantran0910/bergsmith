// Rule vocabulary shared by the list and its dialog: the event kinds the
// coordinator emits, which of them repeat, and how a filter reads in a row.

import type { Schemas } from "../../api/client";
import { shortId } from "../../format";

type EventKind = Schemas["NotificationRule"]["event_kind"];

/** The event kinds a rule may name, in a stable order. */
export const EVENT_KINDS: readonly { value: EventKind; label: string }[] = [
    { value: "job.failed", label: "Job failed" },
    { value: "job.replan_required", label: "Job replan required" },
    {
        value: "publication.commit_outcome_unknown",
        label: "Publication outcome unknown",
    },
    {
        value: "schedule.occurrence_missed",
        label: "Schedule occurrence missed",
    },
    { value: "table.health_critical", label: "Table health critical" },
    {
        value: "workers.pool_without_workers",
        label: "Worker pool without workers",
    },
];

/** The recurring kinds: only these accept a repeat threshold. */
const RECURRING: Record<EventKind, boolean> = {
    "job.failed": false,
    "job.replan_required": true,
    "publication.commit_outcome_unknown": true,
    "schedule.occurrence_missed": true,
    "table.health_critical": false,
    "workers.pool_without_workers": false,
};

/** Whether `kind` may carry a repeat threshold. */
export function isRecurring(kind: EventKind): boolean {
    return RECURRING[kind];
}

/** The human label for an event kind. */
export function eventKindLabel(kind: EventKind): string {
    return EVENT_KINDS.find((option) => option.value === kind)?.label ?? kind;
}

/** The filter modes the form offers, with their user-facing labels. */
export type FilterMode = "none" | "connection" | "table" | "pool";

export const FILTER_MODES: readonly { value: FilterMode; label: string }[] = [
    { value: "none", label: "Any" },
    { value: "connection", label: "Connection" },
    { value: "table", label: "Table" },
    { value: "pool", label: "Worker pool" },
];

/** How a rule's filter reads in a dense list. */
export function filterSummary(
    filter: Schemas["NotificationRuleFilter"] | null,
): string {
    if (filter === null) return "Any";
    if (filter.connection_id !== undefined) {
        return `Connection ${shortId(filter.connection_id)}`;
    }
    if (filter.table_id !== undefined) {
        return `Table ${shortId(filter.table_id)}`;
    }
    if (filter.pool !== undefined) return `Pool ${filter.pool}`;
    return "Any";
}

/** The mode and value a stored filter shows in the form. */
export function filterDraft(filter: Schemas["NotificationRuleFilter"] | null): {
    mode: FilterMode;
    value: string;
} {
    if (filter === null) return { mode: "none", value: "" };
    if (filter.connection_id !== undefined) {
        return { mode: "connection", value: filter.connection_id };
    }
    if (filter.table_id !== undefined) {
        return { mode: "table", value: filter.table_id };
    }
    if (filter.pool !== undefined) {
        return { mode: "pool", value: filter.pool };
    }
    return { mode: "none", value: "" };
}

/**
 * The filter the form names, or `null` for no filter. `undefined` means the
 * value is missing for the chosen mode, so the request is not ready.
 */
export function filterBody(
    mode: FilterMode,
    value: string,
): Schemas["NotificationRuleFilter"] | null | undefined {
    if (mode === "none") return null;
    const trimmed = value.trim();
    if (trimmed === "") return undefined;
    if (mode === "connection") return { connection_id: trimmed };
    if (mode === "table") return { table_id: trimmed };
    return { pool: trimmed };
}

/** One rule's fields while the form holds them. */
export interface RuleDraft {
    name: string;
    destination_id: string;
    event_kind: EventKind;
    filter_mode: FilterMode;
    filter_value: string;
    repeat_threshold: string;
    enabled: boolean;
}

/** A new rule's draft, bound to the first destination the list knows. */
export function newRuleDraft(
    destinations: readonly Schemas["NotificationDestination"][],
): RuleDraft {
    return {
        name: "",
        destination_id: destinations[0]?.id ?? "",
        event_kind: "job.failed",
        filter_mode: "none",
        filter_value: "",
        repeat_threshold: "",
        enabled: true,
    };
}

/** A draft showing a stored rule's current values. */
export function ruleDraftOf(rule: Schemas["NotificationRule"]): RuleDraft {
    const filter = filterDraft(rule.filter);
    return {
        name: rule.name,
        destination_id: rule.destination_id,
        event_kind: rule.event_kind,
        filter_mode: filter.mode,
        filter_value: filter.value,
        repeat_threshold:
            rule.repeat_threshold === null ? "" : String(rule.repeat_threshold),
        enabled: rule.enabled,
    };
}

/**
 * A whole count from the threshold field, or `null` when it is empty. A
 * fractional or negative value is refused, so the form waits for a number
 * the coordinator can store.
 */
function threshold(value: string): number | null | undefined {
    const trimmed = value.trim();
    if (trimmed === "") return null;
    const parsed = Number(trimmed);
    return Number.isSafeInteger(parsed) && parsed > 0 ? parsed : undefined;
}

/** The create request the draft describes, or `null` while it is incomplete. */
export function createRuleBody(
    draft: RuleDraft,
): Schemas["CreateNotificationRule"] | null {
    const name = draft.name.trim();
    const filter = filterBody(draft.filter_mode, draft.filter_value);
    const repeat = threshold(draft.repeat_threshold);
    if (
        name === "" ||
        draft.destination_id === "" ||
        filter === undefined ||
        repeat === undefined
    ) {
        return null;
    }
    const body: Schemas["CreateNotificationRule"] = {
        name,
        destination_id: draft.destination_id,
        event_kind: draft.event_kind,
        filter,
    };
    if (isRecurring(draft.event_kind) && repeat !== null) {
        body.repeat_threshold = repeat;
    }
    return body;
}

/**
 * The update request for `rule`, naming the revision the form read and every
 * editable field so the saved rule is the one the form shows. A conflict
 * surfaces instead of being rebased.
 */
export function updateRuleBody(
    rule: Schemas["NotificationRule"],
    draft: RuleDraft,
): Schemas["UpdateNotificationRule"] | null {
    const name = draft.name.trim();
    const filter = filterBody(draft.filter_mode, draft.filter_value);
    const repeat = threshold(draft.repeat_threshold);
    if (name === "" || filter === undefined || repeat === undefined) {
        return null;
    }
    return {
        revision: rule.revision,
        name,
        destination_id: draft.destination_id,
        filter,
        repeat_threshold: isRecurring(draft.event_kind) ? repeat : null,
        enabled: draft.enabled,
    };
}
