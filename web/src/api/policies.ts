// Maintenance policy reads: organization configuration, paginated in ID
// order like the other lists. Every page is authorized by the server again.

import { useInfiniteQuery, useQueryClient } from "@tanstack/react-query";

import { forSession } from "../queryClient";
import { api, call } from "./client";

/** The organization's policies; disabled policies are listed too. */
export function usePolicies(enabled = true) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["policies", "list"],
        enabled,
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/policies", {
                        params: {
                            query: {
                                limit: 50,
                                ...(pageParam === null
                                    ? {}
                                    : { cursor: pageParam }),
                            },
                        },
                        signal,
                    }),
                ),
            ),
        getNextPageParam: (last) => last.next_cursor,
    });
}
