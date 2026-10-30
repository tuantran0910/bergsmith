// Query hooks over the generated client. Lists follow `next_cursor` with
// "load more"; each page is authorized by the server again. Every query passes
// TanStack's abort signal, so a superseded or cancelled read stops its request,
// and is bound to the session it started in (`forSession`).

import {
    keepPreviousData,
    useInfiniteQuery,
    useQuery,
    useQueryClient,
} from "@tanstack/react-query";

import { jobQueryKey } from "../jobs/events";
import { forSession } from "../queryClient";
import { api, call, type Schemas } from "./client";

const PAGE = 50;

/**
 * How long a filtered list or its facet counts stay fresh. Switching back to
 * a filter within it shows the cached page without a request; a write still
 * invalidates it at once.
 */
const LIST_STALE_MS = 15_000;

/** A filter's query parameters, without the absent ones. */
function present<T extends object>(
    filter: T,
): { [K in keyof T]?: Exclude<T[K], undefined> } {
    return Object.fromEntries(
        Object.entries(filter).filter(([, value]) => value !== undefined),
    ) as { [K in keyof T]?: Exclude<T[K], undefined> };
}

/** One page's query: the first page has no cursor. */
function page(cursor: string | null): { limit: number; cursor?: string } {
    return cursor === null ? { limit: PAGE } : { limit: PAGE, cursor };
}

/**
 * What needs attention across the organization. Job and table figures follow
 * the session's grants; the schedule and worker sections are null when it may
 * not read them.
 */
export function useOverview() {
    const client = useQueryClient();
    return useQuery({
        queryKey: ["overview"],
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(api.GET("/api/v1/overview", { signal })),
            ),
    });
}

export function useCatalogNames() {
    const client = useQueryClient();
    return useQuery({
        queryKey: ["catalogs", "names"],
        queryFn: ({ signal }) =>
            forSession(client, async () => {
                // Catalogs are few; the first page of 100 names the common
                // case and an unnamed catalog falls back to its ID.
                const page = await call(
                    api.GET("/api/v1/catalogs", {
                        params: { query: { limit: 100 } },
                        signal,
                    }),
                );
                return new Map(
                    page.items.map((catalog) => [catalog.id, catalog.name]),
                );
            }),
    });
}

/** The coordinator's table filters (ADR 0067); absent ones are not sent. */
export interface TableFilter {
    health?: "healthy" | "warning" | "critical" | "unassessed" | undefined;
    sort?: "health" | undefined;
    catalog?: string | undefined;
    q?: string | undefined;
}

/**
 * The tables the session may see, worst health first when `health` or the
 * health order is requested. The server evaluates every visible table for
 * those, so the page is not a client-side slice of one page. While a new
 * filter loads, the previous result stays visible (`isPlaceholderData`).
 */
export function useTables(filter: TableFilter = {}) {
    const client = useQueryClient();
    const query = present({ ...filter });
    return useInfiniteQuery({
        queryKey: ["tables", "list", query],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/tables", {
                        params: { query: { ...page(pageParam), ...query } },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
        placeholderData: keepPreviousData,
        staleTime: LIST_STALE_MS,
    });
}

/**
 * Table counts per health state and catalog under the other filters. Pass
 * `enabled: false` while the page shows no filters, so an empty inventory
 * costs no request.
 */
export function useTableFacets(
    filter: Omit<TableFilter, "sort">,
    enabled = true,
) {
    const client = useQueryClient();
    const query = present({ ...filter });
    return useQuery({
        queryKey: ["tables", "facets", query],
        enabled,
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/tables/facets", {
                        params: { query },
                        signal,
                    }),
                ),
            ),
        placeholderData: keepPreviousData,
        staleTime: LIST_STALE_MS,
    });
}

export function useTable(id: string) {
    const client = useQueryClient();
    return useQuery({
        queryKey: ["table", id],
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/tables/{id}", {
                        params: { path: { id } },
                        signal,
                    }),
                ),
            ),
    });
}

/**
 * The table's current definition: its schema, every retained partition spec,
 * its sort order, and its allow-listed properties. The coordinator reads it
 * from catalog metadata on each request, so it is never a stale sample.
 */
export function useTableDefinition(id: string) {
    const client = useQueryClient();
    return useQuery({
        queryKey: ["table", id, "definition"],
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/tables/{id}/definition", {
                        params: { path: { id } },
                        signal,
                    }),
                ),
            ),
    });
}

/**
 * The table's evaluated health: its state, the findings behind it, and the
 * action each finding recommends. Findings need only inventory visibility;
 * an actionable recommendation carries the planner's estimate when the
 * session may submit jobs, and `assessment` makes that explicit.
 */
export function useTableRecommendations(id: string) {
    const client = useQueryClient();
    return useQuery({
        queryKey: ["table", id, "recommendations"],
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/tables/{id}/recommendations", {
                        params: { path: { id } },
                        signal,
                    }),
                ),
            ),
    });
}

/**
 * The table's terminal jobs, newest first, each with the work its accepted
 * tasks rewrote, plus totals over the last thirty days. Totals are returned
 * with every page, so the summary does not wait for the history.
 */
export function useTableMaintenance(id: string) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["table", id, "maintenance"],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/tables/{id}/maintenance", {
                        params: { path: { id }, query: page(pageParam) },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
    });
}

/**
 * The table's newest sample and its history, newest first. The newest sample
 * is returned with every page, so the summary does not wait for the history.
 */
export function useTableStatistics(id: string) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["table", id, "statistics"],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/tables/{id}/statistics", {
                        params: { path: { id }, query: page(pageParam) },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
    });
}

/**
 * The table's recorded commits, newest first, with the commit-per-hour chart
 * over the last day. The chart ends with the current hour, so it answers how
 * busy the table is now rather than over the whole retained history.
 */
export function useTableSnapshots(id: string) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["table", id, "snapshots"],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/tables/{id}/snapshots", {
                        params: {
                            path: { id },
                            query: { ...page(pageParam), hours: 24 },
                        },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
    });
}

/**
 * The coordinator's job filters (ADR 0067). `state` and `operation` are
 * comma-separated values; absent filters are not sent.
 */
export interface JobFilter {
    state?: string | undefined;
    operation?: string | undefined;
    q?: string | undefined;
}

/**
 * The jobs the session may see, newest first unless `order` is `oldest`.
 * While a new filter loads, the previous result stays visible
 * (`isPlaceholderData`).
 */
export function useJobs(
    filter: JobFilter = {},
    order: "newest" | "oldest" = "newest",
) {
    const client = useQueryClient();
    const query = present({ ...filter, order });
    return useInfiniteQuery({
        queryKey: ["jobs", "list", query],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/jobs", {
                        params: {
                            query: { ...page(pageParam), ...query, order },
                        },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
        placeholderData: keepPreviousData,
        staleTime: LIST_STALE_MS,
    });
}

/**
 * Job counts per state and operation under the other filters; `enabled` as
 * for `useTableFacets`.
 */
export function useJobFacets(filter: JobFilter, enabled = true) {
    const client = useQueryClient();
    const query = present({ ...filter });
    return useQuery({
        queryKey: ["jobs", "facets", query],
        enabled,
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/jobs/facets", {
                        params: { query },
                        signal,
                    }),
                ),
            ),
        placeholderData: keepPreviousData,
        staleTime: LIST_STALE_MS,
    });
}

export function useJob(id: string) {
    const client = useQueryClient();
    return useQuery({
        queryKey: jobQueryKey(id),
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/jobs/{id}", {
                        params: { path: { id } },
                        signal,
                    }),
                ),
            ),
    });
}

export function useJobTasks(id: string, enabled: boolean) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["jobs", id, "tasks"],
        initialPageParam: null as string | null,
        enabled,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/jobs/{id}/tasks", {
                        params: { path: { id }, query: page(pageParam) },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
    });
}

export function useWorkers() {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["workers"],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/workers", {
                        params: { query: page(pageParam) },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
        // Lease status changes on its own; keep the list current while open.
        refetchInterval: 15_000,
    });
}

/** The lists that keep saved views. */
export type ViewPage = Schemas["ViewPage"];

/**
 * The session's saved views of one list (ADR 0067). Views are personal, so
 * the query needs no grant; it stays fresh until a save or delete
 * invalidates it.
 */
export function useSavedViews(page: ViewPage) {
    const client = useQueryClient();
    return useQuery({
        queryKey: ["views", page],
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/me/views", {
                        params: { query: { page } },
                        signal,
                    }),
                ),
            ),
        staleTime: Infinity,
    });
}

/** The coordinator's catalog filters (ADR 0067). */
export interface CatalogFilter {
    enabled?: "true" | "false" | undefined;
    q?: string | undefined;
}

/** Catalogs the session may see, under `filter`; see `useTables`. */
export function useCatalogList(filter: CatalogFilter) {
    const client = useQueryClient();
    const query = present({ ...filter });
    return useInfiniteQuery({
        queryKey: ["catalogs", "list", query],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/catalogs", {
                        params: { query: { ...page(pageParam), ...query } },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
        placeholderData: keepPreviousData,
        staleTime: LIST_STALE_MS,
    });
}

/** Catalog counts per `enabled` value under the search. */
export function useCatalogFacets(filter: CatalogFilter, enabled = true) {
    const client = useQueryClient();
    const query = present({ ...filter });
    return useQuery({
        queryKey: ["catalogs", "facets", query],
        enabled,
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/catalogs/facets", {
                        params: { query },
                        signal,
                    }),
                ),
            ),
        placeholderData: keepPreviousData,
        staleTime: LIST_STALE_MS,
    });
}

/**
 * The coordinator's audit filters (ADR 0067). `outcome`, `action`, and
 * `resource_type` are comma-separated; `q` matches the actor's name.
 */
export interface AuditFilter {
    outcome?: string | undefined;
    action?: string | undefined;
    resource_type?: string | undefined;
    actor?: string | undefined;
    q?: string | undefined;
}

/** The organization's audit records under `filter`, newest first by default. */
export function useAuditList(
    filter: AuditFilter,
    order: "newest" | "oldest" = "newest",
) {
    const client = useQueryClient();
    const query = present({ ...filter, order });
    return useInfiniteQuery({
        queryKey: ["audit", "list", query],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/audit", {
                        params: {
                            query: { ...page(pageParam), ...query, order },
                        },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
        placeholderData: keepPreviousData,
        staleTime: LIST_STALE_MS,
    });
}

/** Audit counts per outcome, action, and resource type. */
export function useAuditFacets(filter: AuditFilter, enabled = true) {
    const client = useQueryClient();
    const query = present({ ...filter });
    return useQuery({
        queryKey: ["audit", "facets", query],
        enabled,
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/audit/facets", {
                        params: { query },
                        signal,
                    }),
                ),
            ),
        placeholderData: keepPreviousData,
        staleTime: LIST_STALE_MS,
    });
}

/** The coordinator's principal filters (ADR 0067). */
export interface PrincipalFilter {
    active?: "true" | "false" | undefined;
    q?: string | undefined;
}

/**
 * The organization's users under `filter`. The key sits under `access` so
 * provisioning and deactivation invalidate it with the rest of the directory.
 */
export function useUserList(filter: PrincipalFilter) {
    const client = useQueryClient();
    const query = present({ ...filter });
    return useInfiniteQuery({
        queryKey: ["access", "principals", "users", query],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/principals", {
                        params: {
                            query: {
                                ...page(pageParam),
                                ...query,
                                kind: "User",
                            },
                        },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
        placeholderData: keepPreviousData,
        staleTime: LIST_STALE_MS,
    });
}

/** User counts per `active` value under the search. */
export function useUserFacets(filter: PrincipalFilter, enabled = true) {
    const client = useQueryClient();
    const query = present({ ...filter, kind: "User" as const });
    return useQuery({
        queryKey: ["access", "principals", "facets", query],
        enabled,
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/principals/facets", {
                        params: { query },
                        signal,
                    }),
                ),
            ),
        placeholderData: keepPreviousData,
        staleTime: LIST_STALE_MS,
    });
}
