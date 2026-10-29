// The job pages' URL state (ADR 0056): the detail's open tab and the list's
// filters. Keeping them in the address makes a triage view linkable and
// survives reloads.

import type { Job, JobState } from "../../api/client";
import { joinValues, searchText, valueList } from "../../components/listSearch";

const TABS = ["overview", "tasks"] as const;

export type JobTab = (typeof TABS)[number];

export interface JobSearch {
    tab?: JobTab;
}

/** The tab in a search parameter, or `undefined` for anything else. */
export function jobTab(value: unknown): JobTab | undefined {
    return TABS.find((candidate) => candidate === value);
}

/** Keeps unknown or malformed job-detail search parameters out of the page. */
export function validateJobSearch(search: Record<string, unknown>): JobSearch {
    const selected = jobTab(search["tab"]);
    return selected === undefined ? {} : { tab: selected };
}

/** Job states in lifecycle order, as the state filter offers them. */
export const JOB_STATES = [
    "Planning",
    "Queued",
    "Running",
    "ReadyToCommit",
    "Committing",
    "CommitOutcomeUnknown",
    "Deleting",
    "Cancelling",
    "Succeeded",
    "NoOp",
    "Cancelled",
    "ReplanRequired",
    "Failed",
] as const satisfies readonly JobState[];

/** Operations in the order the operation filter offers them. */
export const JOB_OPERATIONS = [
    "compact",
    "expire_snapshots",
    "remove_orphan_files",
    "rewrite_manifests",
    "remove_dangling_deletes",
] as const satisfies readonly Job["operation"][];

/**
 * The job list's URL state (ADR 0067). Filters are comma-separated values,
 * so a link reads like the API request it makes; the list is newest first
 * unless `order` says otherwise.
 */
export interface JobsSearch {
    q?: string;
    state?: string;
    operation?: string;
    order?: "oldest";
}

/** Keeps unknown or malformed job-list search parameters out of the page. */
export function validateJobsSearch(
    search: Record<string, unknown>,
): JobsSearch {
    const q = searchText(search["q"]);
    const state = joinValues(valueList(search["state"], JOB_STATES));
    const operation = joinValues(
        valueList(search["operation"], JOB_OPERATIONS),
    );
    return {
        ...(q !== undefined ? { q } : {}),
        ...(state !== undefined ? { state } : {}),
        ...(operation !== undefined ? { operation } : {}),
        ...(search["order"] === "oldest" ? { order: "oldest" as const } : {}),
    };
}
