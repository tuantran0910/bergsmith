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

/**
 * A namespace's canonical URL key. Iceberg keeps namespace levels separate
 * and a level may contain a dot, so the key is JSON rather than a flattening
 * join (ADR 0056).
 */
export function namespaceKey(levels: readonly string[]): string {
    return JSON.stringify(levels);
}

/** The levels in a namespace key, or `undefined` for a malformed one. */
export function parseNamespaceKey(value: string): string[] | undefined {
    try {
        const parsed: unknown = JSON.parse(value);
        return Array.isArray(parsed) &&
            parsed.every((level) => typeof level === "string")
            ? parsed
            : undefined;
    } catch {
        return undefined;
    }
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
    const levels =
        namespace === undefined ? undefined : parseNamespaceKey(namespace);
    const sort = sortColumn(search["sort"]);
    const order =
        search["order"] === "asc" || search["order"] === "desc"
            ? search["order"]
            : undefined;
    return {
        ...(q !== undefined ? { q } : {}),
        ...(catalog !== undefined ? { catalog } : {}),
        ...(levels !== undefined ? { namespace: namespaceKey(levels) } : {}),
        ...(sort !== undefined ? { sort } : {}),
        ...(order !== undefined ? { order } : {}),
    };
}
