import { useQuery, useQueryClient } from "@tanstack/react-query";
import { forSession } from "../queryClient";
import { api, call } from "./client";

export function useTableStorage(id: string) {
    const client = useQueryClient();
    return useQuery({
        queryKey: ["table", id, "storage"],
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/tables/{id}/storage", {
                        params: { path: { id } },
                        signal,
                    }),
                ),
            ),
    });
}
export function useCatalogStorage(id: string) {
    const client = useQueryClient();
    return useQuery({
        queryKey: ["catalog", id, "storage"],
        queryFn: ({ signal }) =>
            forSession(client, () =>
                call(
                    api.GET("/api/v1/catalogs/{id}/storage", {
                        params: { path: { id } },
                        signal,
                    }),
                ),
            ),
    });
}
