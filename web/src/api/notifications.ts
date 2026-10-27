// Notification administration reads and the delivery log. Destinations and
// rules are organization configuration; deliveries are a bounded, newest-first
// log. Every request is authorized by the server again.

import {
    useInfiniteQuery,
    useQuery,
    useQueryClient,
} from "@tanstack/react-query";

import { forSession } from "../queryClient";
import { api, call } from "./client";

/** The organization's destinations, including disabled ones. */
export function useNotificationDestinations(enabled = true) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["notifications", "destinations"],
        enabled,
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/notifications/destinations", {
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

/** The organization's rules, including disabled ones. */
export function useNotificationRules(enabled = true) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["notifications", "rules"],
        enabled,
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/notifications/rules", {
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

/** The delivery-log filters the API accepts. */
export interface DeliveryFilters {
    state?: "pending" | "delivered" | "exhausted";
    ruleId?: string;
    destinationId?: string;
}

/** The organization's deliveries, newest first. */
export function useNotificationDeliveries(
    filters: DeliveryFilters,
    enabled = true,
) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["notifications", "deliveries", filters],
        enabled,
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/notifications/deliveries", {
                        params: {
                            query: {
                                limit: 50,
                                ...(filters.state === undefined
                                    ? {}
                                    : { state: filters.state }),
                                ...(filters.ruleId === undefined
                                    ? {}
                                    : { rule_id: filters.ruleId }),
                                ...(filters.destinationId === undefined
                                    ? {}
                                    : {
                                          destination_id: filters.destinationId,
                                      }),
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

/** One delivery with its attempt history. */
export function useNotificationDelivery(id: string, enabled = true) {
    const client = useQueryClient();
    return useQuery({
        queryKey: ["notifications", "deliveries", "detail", id],
        enabled,
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/notifications/deliveries/{id}", {
                        params: { path: { id } },
                        signal,
                    }),
                ),
            ),
    });
}
