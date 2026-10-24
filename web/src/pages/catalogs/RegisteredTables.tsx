import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { api, call, csrfHeader, type Schemas } from "../../api/client";
import { useTables } from "../../api/queries";
import { ErrorNotice } from "../../components/ErrorNotice";
import { LoadMore, LoadingRows } from "../../components/Page";
import { useSessionMutation } from "../../queryClient";
import { Button } from "../../ui/Button";
import { Card, CardContent, CardHeader, CardTitle } from "../../ui/Card";
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogTitle,
} from "../../ui/Dialog";

/** Ledger names remain removable even if the catalog no longer lists them. */
export function RegisteredTables({
    id,
    manager,
}: {
    id: string;
    manager: boolean;
}) {
    const tables = useTables();
    const client = useQueryClient();
    const [selected, setSelected] = useState<Schemas["TableAlias"] | null>(
        null,
    );
    const remove = useSessionMutation({
        mutationFn: (alias: Schemas["TableAlias"]) =>
            call(
                api.DELETE("/api/v1/catalogs/{id}/registrations", {
                    params: {
                        path: { id },
                        header: csrfHeader(),
                        query: {
                            namespace: alias.namespace,
                            table: alias.name,
                            revision: alias.revision,
                        },
                    },
                }),
            ),
        onSuccess: () => {
            setSelected(null);
            void client.invalidateQueries({ queryKey: ["tables"] });
            void client.invalidateQueries({
                queryKey: ["catalog", id, "discovered-tables"],
            });
        },
    });
    const items =
        tables.data?.pages.flatMap((page) =>
            page.items.flatMap((table) =>
                table.aliases
                    .filter((alias) => alias.catalog_id === id)
                    .map((alias) => ({ table, alias })),
            ),
        ) ?? [];
    return (
        <Card>
            <CardHeader>
                <CardTitle>Registered tables</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-3">
                {tables.isPending && (
                    <LoadingRows label="Loading registered tables…" />
                )}
                {tables.error && (
                    <ErrorNotice
                        error={tables.error}
                        onRetry={() => {
                            void tables.refetch();
                        }}
                    />
                )}
                {tables.isSuccess && items.length === 0 && (
                    <p>
                        {tables.hasNextPage
                            ? "No matching registrations on the loaded pages. Load more to continue."
                            : "No tables are registered on this connection."}
                    </p>
                )}
                <ul className="m-0 grid list-none gap-3 p-0">
                    {items.map(({ table, alias }) => (
                        <li
                            key={JSON.stringify([alias.namespace, alias.name])}
                            className="flex flex-wrap items-center justify-between gap-3"
                        >
                            <Link
                                to="/tables/$tableId"
                                params={{ tableId: table.id }}
                            >
                                {[...alias.namespace, alias.name].join(".")}
                            </Link>
                            {manager && (
                                <Button
                                    variant="outline"
                                    size="sm"
                                    onClick={() => {
                                        remove.reset();
                                        setSelected(alias);
                                    }}
                                    aria-label={`Remove registration ${[...alias.namespace, alias.name].join(".")}`}
                                >
                                    Remove registration
                                </Button>
                            )}
                        </li>
                    ))}
                </ul>
                {tables.isSuccess && (
                    <LoadMore
                        shown={items.length}
                        noun="registered tables"
                        hasMore={tables.hasNextPage}
                        loading={tables.isFetchingNextPage}
                        onLoad={() => {
                            void tables.fetchNextPage();
                        }}
                    />
                )}
                <Dialog
                    open={selected !== null}
                    onOpenChange={(open) => {
                        if (!open && !remove.isPending) setSelected(null);
                    }}
                >
                    <DialogContent showCloseButton={!remove.isPending}>
                        <DialogTitle>Remove registration?</DialogTitle>
                        <DialogDescription>
                            Remove{" "}
                            {selected &&
                                [...selected.namespace, selected.name].join(
                                    ".",
                                )}{" "}
                            from this connection. The Iceberg table, data, other
                            aliases, and job history remain. Active jobs can
                            prevent removal.
                        </DialogDescription>
                        {remove.error !== null && (
                            <ErrorNotice error={remove.error} />
                        )}
                        {remove.error !== null && (
                            <p>
                                Close this dialog and refresh the registered
                                tables before trying again.
                            </p>
                        )}
                        <Button
                            variant="destructive"
                            disabled={remove.isPending || remove.error !== null}
                            onClick={() => {
                                if (selected) remove.mutate(selected);
                            }}
                        >
                            {remove.isPending ? "Removing…" : "Confirm removal"}
                        </Button>
                        <Button
                            variant="outline"
                            disabled={remove.isPending}
                            onClick={() => {
                                setSelected(null);
                                void tables.refetch();
                            }}
                        >
                            Close
                        </Button>
                    </DialogContent>
                </Dialog>
            </CardContent>
        </Card>
    );
}
