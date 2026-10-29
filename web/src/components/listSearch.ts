// URL state shared by filtered lists (ADR 0067): free-text search and
// multi-value filters, validated the way the coordinator validates them, so a
// link never carries a filter the API would reject.

/** The coordinator's limit on `q`, in characters. */
export const MAX_SEARCH = 128;

/** The coordinator's limit on one filter's comma-separated values. */
export const MAX_VALUES = 16;

/** The search field's typing pause before the list follows it. */
export const SEARCH_DELAY_MS = 150;

/**
 * Search text the coordinator accepts: control characters removed, trimmed,
 * and at most 128 characters; `undefined` when nothing remains.
 */
export function searchText(value: unknown): string | undefined {
    if (typeof value !== "string") {
        return undefined;
    }
    // eslint-disable-next-line no-control-regex -- removing them is the point
    const cleaned = value.replace(/[\u0000-\u001f\u007f-\u009f]/g, "").trim();
    if (cleaned === "") {
        return undefined;
    }
    // Characters, not UTF-16 units, as the coordinator counts them.
    return Array.from(cleaned).slice(0, MAX_SEARCH).join("").trim();
}

/**
 * The known values in a comma-separated parameter, in `allowed` order and
 * without duplicates. Unknown values are dropped rather than sent, since the
 * coordinator rejects them.
 */
export function valueList<T extends string>(
    value: unknown,
    allowed: readonly T[],
): T[] {
    if (typeof value !== "string") {
        return [];
    }
    const given = new Set(value.split(","));
    return allowed.filter((item) => given.has(item)).slice(0, MAX_VALUES);
}

/** A list parameter's canonical form, or `undefined` for no values. */
export function joinValues(values: readonly string[]): string | undefined {
    return values.length === 0 ? undefined : values.join(",");
}

/** `values` with `value` added or removed, keeping `allowed` order. */
export function toggleValue<T extends string>(
    values: readonly T[],
    value: T,
    allowed: readonly T[],
): T[] {
    const next = new Set(values);
    if (next.has(value)) {
        next.delete(value);
    } else {
        next.add(value);
    }
    return allowed.filter((item) => next.has(item));
}
