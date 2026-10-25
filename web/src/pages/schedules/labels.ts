// Names for the resources a schedule points at. Schedules carry identifiers;
// the page names them through the policy and table lists, and falls back to a
// short identifier when the caller may not read them.

import { useMemo } from "react";

import type { Schemas } from "../../api/client";
import { useCatalogNames, useTables } from "../../api/queries";
import { usePolicies } from "../../api/policies";
import { shortId, tableName } from "../../format";

export function useScheduleLabels(enabled: boolean) {
    const policies = usePolicies(enabled);
    const tables = useTables();
    const catalogs = useCatalogNames();
    const names = catalogs.data ?? new Map<string, string>();
    const policyItems = useMemo(
        () => policies.data?.pages.flatMap((page) => page.items) ?? [],
        [policies.data],
    );
    const tablesById = useMemo(() => {
        const found = new Map<string, Schemas["Table"]>();
        for (const page of tables.data?.pages ?? []) {
            for (const table of page.items) {
                found.set(table.id, table);
            }
        }
        return found;
    }, [tables.data]);

    return {
        policies: policyItems,
        tables: [...tablesById.values()],
        policyName: (id: string) =>
            policyItems.find((policy) => policy.id === id)?.name ??
            `Policy ${shortId(id)}`,
        tableName: (id: string) => {
            const table = tablesById.get(id);
            return table === undefined
                ? `Table ${shortId(id)}`
                : tableName(table.aliases, id, names);
        },
        /** The table list has more pages the picker can load. */
        moreTables: tables.hasNextPage,
        loadingTables: tables.isFetchingNextPage,
        loadMoreTables: () => {
            void tables.fetchNextPage();
        },
    };
}
