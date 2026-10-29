// Query hooks over the generated client. Lists follow `next_cursor` with
// "load more"; each page is authorized by the server again. Every query passes
// TanStack's abort signal, so a superseded or cancelled read stops its request,
// and is bound to the session it started in (`forSession`).

import {
    useInfiniteQuery,
    useQuery,
    useQueryClient,
} from "@tanstack/react-query";

import { jobQueryKey } from "../jobs/events";
import { forSession } from "../queryClient";
import { api, call } from "./client";

const PAGE = 50;

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

/**
 * The tables the session may see, worst health first when `health` or the
 * health order is requested. The server evaluates every visible table for
 * those, so the page is not a client-side slice of one page.
 */
export function useTables({
    health,
    sort,
}: {
    health?: "healthy" | "warning" | "critical" | "unassessed" | undefined;
    sort?: "health" | undefined;
} = {}) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["tables", health ?? "any", sort ?? "name"],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/tables", {
                        params: {
                            query: {
                                ...page(pageParam),
                                ...(health === undefined ? {} : { health }),
                                ...(sort === undefined ? {} : { sort }),
                            },
                        },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
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

export function useJobs() {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["jobs"],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/jobs", {
                        params: { query: page(pageParam) },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
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

export function useAudit() {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["audit"],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/audit", {
                        params: { query: page(pageParam) },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
    });
}
