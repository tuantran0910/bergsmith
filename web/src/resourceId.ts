// Resource identifiers in addresses: routes and search parameters check the
// shape before asking the coordinator for one.

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** Whether `value` has the shape of a resource ID, so it is worth asking for. */
export function isResourceId(value: string): boolean {
    return UUID.test(value);
}

/** A search parameter holding a resource ID, or `undefined` for anything else. */
export function resourceIdParam(value: unknown): string | undefined {
    return typeof value === "string" && isResourceId(value) ? value : undefined;
}
