// The job detail's URL state (ADR 0056): which tab is open. Keeping it in the
// address makes a triage view linkable and survives reloads.

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
