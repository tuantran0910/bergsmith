// The principal detail's URL state (ADR 0056): which tab is open. Tabs stay in
// validated search parameters so a link opens the same view, while an unknown
// tab falls back to access.

const TABS = ["access", "tokens"] as const;

/** The principal detail's addressable tab. */
export type PrincipalTab = (typeof TABS)[number];

export interface PrincipalSearch {
    tab?: PrincipalTab;
}

/** The tab in a search parameter, or `undefined` for anything else. */
export function principalTab(value: unknown): PrincipalTab | undefined {
    return TABS.find((candidate) => candidate === value);
}

/** Keeps unknown or malformed principal-detail search parameters out of the page. */
export function validatePrincipalSearch(
    search: Record<string, unknown>,
): PrincipalSearch {
    const selected = principalTab(search["tab"]);
    return selected === undefined ? {} : { tab: selected };
}
