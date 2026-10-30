// The audit log's URL state (ADR 0067): filters, actor search, and order in
// validated search parameters, so a triage link reopens the same records.

import {
    joinValues,
    searchText,
    tokenList,
    valueList,
} from "../../components/listSearch";
import { resourceIdParam } from "../../resourceId";

/** Outcomes in the order the filter offers them. */
export const AUDIT_OUTCOMES = ["Allowed", "Denied", "Failed"] as const;

/** Stable action names such as `job.admitted`. */
const ACTION = /^[a-z][a-z_]*\.[a-z][a-z_]*$/;

/** Resource kinds such as `job`, as the coordinator accepts them. */
const RESOURCE_TYPE = /^[a-z_]{1,100}$/;

export interface AuditSearch {
    q?: string;
    outcome?: string;
    action?: string;
    resource_type?: string;
    /** Records by this principal. */
    actor?: string;
    /** Newest first unless `oldest`. */
    order?: "oldest";
}

/** Keeps unknown or malformed audit search parameters out of the page. */
export function validateAuditSearch(
    search: Record<string, unknown>,
): AuditSearch {
    const q = searchText(search["q"]);
    const outcome = joinValues(valueList(search["outcome"], AUDIT_OUTCOMES));
    const action = joinValues(tokenList(search["action"], ACTION));
    const resourceType = joinValues(
        tokenList(search["resource_type"], RESOURCE_TYPE),
    );
    const actor = resourceIdParam(search["actor"]);
    return {
        ...(q !== undefined ? { q } : {}),
        ...(outcome !== undefined ? { outcome } : {}),
        ...(action !== undefined ? { action } : {}),
        ...(resourceType !== undefined ? { resource_type: resourceType } : {}),
        ...(actor !== undefined ? { actor } : {}),
        ...(search["order"] === "oldest" ? { order: "oldest" as const } : {}),
    };
}
