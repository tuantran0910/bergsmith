// Schedule reads: organization configuration plus the durable evaluation
// history. Every page is authorized by the server again, and configuration is
// read on demand rather than polled.

import {
    useInfiniteQuery,
    useQuery,
    useQueryClient,
} from "@tanstack/react-query";

import { forSession } from "../queryClient";
import { api, call } from "./client";

const PAGE = 50;

function page(cursor: string | null): { limit: number; cursor?: string } {
    return cursor === null ? { limit: PAGE } : { limit: PAGE, cursor };
}

/** The organization's schedules; disabled schedules are listed too. */
export function useSchedules(enabled = true) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["schedules", "list"],
        enabled,
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/schedules", {
                        params: { query: page(pageParam) },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
    });
}

/** One schedule, including its derived next instants and identity name. */
export function useSchedule(id: string) {
    const client = useQueryClient();
    return useQuery({
        queryKey: ["schedule", id],
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/schedules/{id}", {
                        params: { path: { id } },
                        signal,
                    }),
                ),
            ),
    });
}

/** The schedule's evaluations, newest first. */
export function useScheduleOccurrences(id: string, enabled = true) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["schedule", id, "occurrences"],
        enabled,
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/schedules/{id}/occurrences", {
                        params: { path: { id }, query: page(pageParam) },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
    });
}
