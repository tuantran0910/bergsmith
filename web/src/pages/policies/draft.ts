// A policy's editable fields as its form holds them, and the requests those
// fields build. Numbers stay text until submission so an emptied field is
// visible rather than silently becoming zero.

import type { Schemas } from "../../api/client";

/** One policy's fields while they are being edited. */
export interface PolicyDraft {
    name: string;
    target: string;
    minimum: string;
    budget: string;
    enabled: boolean;
}

const MIB = 1024 * 1024;
const GIB = 1024 * MIB;

/**
 * The values `POST /api/v1/policies` applies when a create names none, so the
 * form opens on the policy the coordinator would have created anyway.
 */
export const DEFAULT_TARGET_BYTES = 256 * MIB;
export const DEFAULT_MINIMUM_FILES = 5;
export const DEFAULT_BUDGET_BYTES = 100 * GIB;

/** A draft for a new policy: binpack compaction at the planner defaults. */
export function newDraft(): PolicyDraft {
    return {
        name: "",
        target: String(DEFAULT_TARGET_BYTES),
        minimum: String(DEFAULT_MINIMUM_FILES),
        budget: String(DEFAULT_BUDGET_BYTES),
        enabled: true,
    };
}

/** A draft showing a stored policy's current values. */
export function draftOf(policy: Schemas["MaintenancePolicy"]): PolicyDraft {
    return {
        name: policy.name,
        target: String(policy.target_file_size_bytes),
        minimum: String(policy.min_input_files),
        budget: String(policy.max_input_bytes),
        enabled: policy.enabled,
    };
}

/** A whole number from the field, or `null` when it is empty or fractional. */
function whole(value: string): number | null {
    const parsed = Number(value);
    return Number.isSafeInteger(parsed) && parsed > 0 ? parsed : null;
}

/**
 * The create request the draft describes, or `null` while a field is empty.
 * The coordinator checks the documented ranges and their relationship; the
 * browser only refuses to send a value it cannot express.
 */
export function createBody(
    draft: PolicyDraft,
): Schemas["MaintenancePolicyCreate"] | null {
    const name = draft.name.trim();
    const target = whole(draft.target);
    const minimum = whole(draft.minimum);
    const budget = whole(draft.budget);
    if (name === "" || target === null || minimum === null || budget === null) {
        return null;
    }
    return {
        name,
        target_file_size_bytes: target,
        min_input_files: minimum,
        max_input_bytes: budget,
    };
}

/**
 * The update request for `policy`, naming the revision the form read. Only
 * changed fields are sent; `null` means the draft is incomplete.
 */
export function updateBody(
    policy: Schemas["MaintenancePolicy"],
    draft: PolicyDraft,
): Schemas["MaintenancePolicyUpdate"] | null {
    const next = createBody(draft);
    if (next === null) {
        return null;
    }
    const changes: Schemas["MaintenancePolicyUpdate"] = {
        revision: policy.revision,
    };
    if (next.name !== policy.name) changes.name = next.name;
    const target = next.target_file_size_bytes;
    if (target !== undefined && target !== policy.target_file_size_bytes) {
        changes.target_file_size_bytes = target;
    }
    const minimum = next.min_input_files;
    if (minimum !== undefined && minimum !== policy.min_input_files) {
        changes.min_input_files = minimum;
    }
    const budget = next.max_input_bytes;
    if (budget !== undefined && budget !== policy.max_input_bytes) {
        changes.max_input_bytes = budget;
    }
    if (draft.enabled !== policy.enabled) changes.enabled = draft.enabled;
    return changes;
}

/** Whether the draft names any change at all. */
export function changed(
    policy: Schemas["MaintenancePolicy"],
    draft: PolicyDraft,
): boolean {
    const body = updateBody(policy, draft);
    return (
        body !== null && Object.keys(body).some((field) => field !== "revision")
    );
}
