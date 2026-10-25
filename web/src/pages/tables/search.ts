// The table list's URL state (ADR 0056): search, filters, and sort live in
// validated search parameters so links, back/forward, and saved views work.

const SORTS = ["name", "catalog", "namespace", "registered"] as const;

export type TableSort = (typeof SORTS)[number];

export interface TablesSearch {
    q?: string;
    catalog?: string;
    namespace?: string;
    sort?: TableSort;
    order?: "asc" | "desc";
}

function text(value: unknown): string | undefined {
    return typeof value === "string" && value.trim() !== "" ? value : undefined;
}

export function sortColumn(value: unknown): TableSort | undefined {
    return SORTS.find((candidate) => candidate === value);
}

/** Keeps unknown or malformed search parameters out of the page. */
export function validateTablesSearch(
    search: Record<string, unknown>,
): TablesSearch {
    const q = text(search["q"]);
    const catalog = text(search["catalog"]);
    const namespace = text(search["namespace"]);
    const sort = sortColumn(search["sort"]);
    const order =
        search["order"] === "asc" || search["order"] === "desc"
            ? search["order"]
            : undefined;
    return {
        ...(q !== undefined ? { q } : {}),
        ...(catalog !== undefined ? { catalog } : {}),
        ...(namespace !== undefined ? { namespace } : {}),
        ...(sort !== undefined ? { sort } : {}),
        ...(order !== undefined ? { order } : {}),
    };
}
