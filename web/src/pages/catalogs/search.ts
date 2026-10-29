// The catalog detail's URL state (ADR 0056): which tab is open. Tabs stay in
// validated search parameters so a link opens the same view, while an unknown
// tab falls back to the overview.

const TABS = ["overview", "tables", "access", "settings"] as const;

/** The catalog detail's addressable tab. */
export type CatalogTab = (typeof TABS)[number];

export interface CatalogSearch {
    tab?: CatalogTab;
}

/** The tab in a search parameter, or `undefined` for anything else. */
export function catalogTab(value: unknown): CatalogTab | undefined {
    return TABS.find((candidate) => candidate === value);
}

/** Keeps unknown or malformed catalog-detail search parameters out of the page. */
export function validateCatalogSearch(
    search: Record<string, unknown>,
): CatalogSearch {
    const selected = catalogTab(search["tab"]);
    return selected === undefined ? {} : { tab: selected };
}
