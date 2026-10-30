// The table list's URL state (ADR 0056): search, filters, and sort live in
// validated search parameters so links, back/forward, and saved views work.

import { searchText } from "../../components/listSearch";
import { resourceIdParam } from "../../resourceId";

const SORTS = [
    "name",
    "catalog",
    "namespace",
    "health",
    "registered",
    "records",
    "size",
    "files",
    "small",
] as const;

export type TableSort = (typeof SORTS)[number];

const TABS = [
    "overview",
    "definition",
    "health",
    "snapshots",
    "statistics",
    "maintenance",
    "storage",
    "access",
] as const;

/** The table detail's addressable tab. */
export type TableTab = (typeof TABS)[number];

/** The table detail's URL state: which tab is open. */
export interface TableSearch {
    tab?: TableTab;
}

/** The evaluated states a table list can be filtered to. */
const HEALTH = ["healthy", "warning", "critical", "unassessed"] as const;

export type TableHealthFilter = (typeof HEALTH)[number];

/**
 * The coordinator filters by `q`, `catalog`, and `health` (ADR 0067); `q`
 * matches a visible alias's `namespace.name`, so it also finds a namespace.
 */
export interface TablesSearch {
    q?: string;
    /** The table previewed beside the list. */
    peek?: string;
    catalog?: string;
    health?: TableHealthFilter | undefined;
    sort?: TableSort;
    order?: "asc" | "desc";
}

/** The filter in a search parameter, or `undefined` for anything else. */
export function healthFilter(value: unknown): TableHealthFilter | undefined {
    return HEALTH.find((candidate) => candidate === value);
}

function text(value: unknown): string | undefined {
    return typeof value === "string" && value.trim() !== "" ? value : undefined;
}

/**
 * A namespace's canonical key. Iceberg keeps namespace levels separate and a
 * level may contain a dot, so the key is JSON rather than a flattening join
 * (ADR 0056).
 */
export function namespaceKey(levels: readonly string[]): string {
    return JSON.stringify(levels);
}

export function sortColumn(value: unknown): TableSort | undefined {
    return SORTS.find((candidate) => candidate === value);
}

/** The tab in a search parameter, or `undefined` for anything else. */
export function tab(value: unknown): TableTab | undefined {
    return TABS.find((candidate) => candidate === value);
}

/** Keeps unknown or malformed table-detail search parameters out of the page. */
export function validateTableSearch(
    search: Record<string, unknown>,
): TableSearch {
    const selected = tab(search["tab"]);
    return selected === undefined ? {} : { tab: selected };
}

/** Keeps unknown or malformed search parameters out of the page. */
export function validateTablesSearch(
    search: Record<string, unknown>,
): TablesSearch {
    const q = searchText(search["q"]);
    const peek = resourceIdParam(search["peek"]);
    const catalog = text(search["catalog"]);
    const health = healthFilter(search["health"]);
    const sort = sortColumn(search["sort"]);
    const order =
        search["order"] === "asc" || search["order"] === "desc"
            ? search["order"]
            : undefined;
    return {
        ...(q !== undefined ? { q } : {}),
        ...(peek !== undefined ? { peek } : {}),
        ...(catalog !== undefined ? { catalog } : {}),
        ...(health !== undefined ? { health } : {}),
        ...(sort !== undefined ? { sort } : {}),
        ...(order !== undefined ? { order } : {}),
    };
}
