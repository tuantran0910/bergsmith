import { useId } from "react";
import { pageParams, useAccessPage } from "../../api/access";
import { api, call } from "../../api/client";
import { ErrorNotice } from "../../components/ErrorNotice";
import { LoadMore } from "../../components/Page";
import { Input, Label } from "../../ui/Input";

/** Suggestions preserve canonical IDs while letting operators find resources by name. */
export function ScopeResource({
    kind,
    value,
    onChange,
}: {
    kind: string;
    value: string;
    onChange: (value: string) => void;
}) {
    const list = useId();
    const query = useAccessPage(
        ["scope-resources", kind],
        async (cursor, signal) => {
            if (kind === "connection") {
                const page = await call(
                    api.GET("/api/v1/catalogs", {
                        params: { query: pageParams(cursor) },
                        signal,
                    }),
                );
                return {
                    ...page,
                    items: page.items.map((item) => ({
                        id: item.id,
                        name: item.name,
                    })),
                };
            }
            const page = await call(
                api.GET("/api/v1/tables", {
                    params: { query: pageParams(cursor) },
                    signal,
                }),
            );
            return {
                ...page,
                items: page.items.map((item) => ({
                    id: item.id,
                    name:
                        item.aliases
                            .map((alias) =>
                                [...alias.namespace, alias.name].join("."),
                            )
                            .join(", ") || item.id,
                })),
            };
        },
    );
    const items = query.data?.pages.flatMap((page) => page.items) ?? [];
    return (
        <div className="grid gap-2">
            <Label>
                {kind === "table" ? "Table ID" : "Connection ID"}
                <Input
                    required
                    list={list}
                    value={value}
                    onChange={(event) => {
                        onChange(event.target.value);
                    }}
                />
            </Label>
            <datalist id={list}>
                {items.map((item) => (
                    <option key={item.id} value={item.id}>
                        {item.name}
                    </option>
                ))}
            </datalist>
            <p className="m-0 text-sm text-muted-foreground">
                Choose a named resource from the suggestions or paste its ID.
            </p>
            {query.isPending && (
                <p role="status">Loading resource suggestions…</p>
            )}
            {query.error && (
                <ErrorNotice
                    error={query.error}
                    onRetry={() => {
                        void query.refetch();
                    }}
                />
            )}
            <LoadMore
                shown={items.length}
                noun="resource suggestions"
                hasMore={query.hasNextPage}
                loading={query.isFetchingNextPage}
                onLoad={() => {
                    void query.fetchNextPage();
                }}
            />
        </div>
    );
}
