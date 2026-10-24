// Connection queries share the session fencing and cursor policy of inventory.
import {
    useInfiniteQuery,
    useQuery,
    useQueryClient,
} from "@tanstack/react-query";
import { forSession } from "../queryClient";
import { api, call } from "./client";

export function useCatalogs() {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["catalogs", "list"],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/catalogs", {
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

export function useCatalog(id: string) {
    const client = useQueryClient();
    return useQuery({
        queryKey: ["catalog", id],
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/catalogs/{id}", {
                        params: { path: { id } },
                        signal,
                    }),
                ),
            ),
    });
}

/** Lists one level at a time; namespace levels are serialized as repeated parameters. */
export function useNamespaces(id: string, parent: string[]) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["catalog", id, "namespaces", parent],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/catalogs/{id}/namespaces", {
                        params: {
                            path: { id },
                            query: {
                                parent,
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

export function useCatalogTables(id: string, namespace: string[]) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["catalog", id, "discovered-tables", namespace],
        enabled: namespace.length > 0,
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/catalogs/{id}/tables", {
                        params: {
                            path: { id },
                            query: {
                                namespace,
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
