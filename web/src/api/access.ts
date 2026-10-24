import {
    useInfiniteQuery,
    useQuery,
    useQueryClient,
} from "@tanstack/react-query";
import { forSession } from "../queryClient";
import { api, call, type Schemas } from "./client";

export type AccessScope = Schemas["GrantScope"];

/** Cursor reads stay bound to the signed-in identity and their complete filters. */
export function useAccessPage<T extends { next_cursor: string | null }>(
    key: readonly unknown[],
    read: (cursor: string | null, signal: AbortSignal) => Promise<T>,
) {
    const client = useQueryClient();
    return useInfiniteQuery({
        queryKey: ["access", ...key],
        initialPageParam: null as string | null,
        queryFn: ({ pageParam, signal }) =>
            forSession(client, () => read(pageParam, signal)),
        getNextPageParam: (last) => last.next_cursor,
    });
}

export function pageParams(cursor: string | null) {
    return { limit: 50, ...(cursor === null ? {} : { cursor }) };
}

export function usePrincipals(services: boolean) {
    return useAccessPage(["principals", services], (cursor, signal) =>
        services
            ? call(
                  api.GET("/api/v1/service-identities", {
                      params: { query: pageParams(cursor) },
                      signal,
                  }),
              )
            : call(
                  api.GET("/api/v1/principals", {
                      params: {
                          query: { ...pageParams(cursor), kind: "User" },
                      },
                      signal,
                  }),
              ),
    );
}

export function usePrincipal(id: string, services: boolean) {
    const client = useQueryClient();
    return useQuery({
        queryKey: ["access", "principal", id, services],
        queryFn: ({ signal }) =>
            forSession(client, () =>
                services
                    ? call(
                          api.GET("/api/v1/service-identities/{id}", {
                              params: { path: { id } },
                              signal,
                          }),
                      )
                    : call(
                          api.GET("/api/v1/principals/{id}", {
                              params: { path: { id } },
                              signal,
                          }),
                      ),
            ),
    });
}
