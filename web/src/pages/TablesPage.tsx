import { Link } from "@tanstack/react-router";
import { Table2Icon } from "lucide-react";

import { useCatalogNames, useTables } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import {
    EmptyState,
    GridFrame,
    LoadMore,
    LoadingRows,
    PageHeader,
} from "../components/Page";
import { ResourceId } from "../components/ResourceId";
import { aliasName, timestamp } from "../format";
import {
    Table,
    TableBody,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "../ui/Table";

/** Canonical tables the session may see, named through their aliases. */
export function TablesPage() {
    const tables = useTables();
    const catalogs = useCatalogNames();
    const names = catalogs.data ?? new Map<string, string>();
    const items = tables.data?.pages.flatMap((page) => page.items) ?? [];

    return (
        <section>
            <PageHeader
                title="Tables"
                description="Iceberg tables you may maintain, by catalog, namespace, and name."
            />
            {tables.isPending && <LoadingRows label="Loading tables…" />}
            {tables.error && (
                <ErrorNotice
                    error={tables.error}
                    onRetry={() => {
                        void tables.refetch();
                    }}
                />
            )}
            {tables.isSuccess && items.length === 0 && (
                <EmptyState
                    icon={Table2Icon}
                    title="No tables are visible to you."
                >
                    A table appears here once its catalog is connected and you
                    hold a grant on the catalog or the table.
                </EmptyState>
            )}
            {items.length > 0 && (
                <>
                    <GridFrame>
                        <Table>
                            <TableHeader>
                                <TableRow>
                                    <TableHead className="pl-4">
                                        Table UUID
                                    </TableHead>
                                    <TableHead>Table</TableHead>
                                    <TableHead>Other names</TableHead>
                                    <TableHead className="pr-4">
                                        Registered
                                    </TableHead>
                                </TableRow>
                            </TableHeader>
                            <TableBody>
                                {items.map((table) => {
                                    const [primary, ...others] = table.aliases;
                                    return (
                                        <TableRow key={table.id}>
                                            <TableCell className="pl-4">
                                                <ResourceId
                                                    id={table.table_uuid}
                                                    label="table UUID"
                                                    short
                                                />
                                            </TableCell>
                                            <TableCell className="font-medium">
                                                <Link
                                                    to="/tables/$tableId"
                                                    params={{
                                                        tableId: table.id,
                                                    }}
                                                >
                                                    {primary
                                                        ? aliasName(
                                                              primary,
                                                              names,
                                                          )
                                                        : table.id}
                                                </Link>
                                            </TableCell>
                                            <TableCell className="text-muted-foreground">
                                                {others
                                                    .map((alias) =>
                                                        aliasName(alias, names),
                                                    )
                                                    .join(", ")}
                                            </TableCell>
                                            <TableCell className="pr-4 whitespace-nowrap text-muted-foreground">
                                                {timestamp(table.created_at)}
                                            </TableCell>
                                        </TableRow>
                                    );
                                })}
                            </TableBody>
                        </Table>
                    </GridFrame>
                    <LoadMore
                        shown={items.length}
                        noun="tables"
                        hasMore={tables.hasNextPage}
                        loading={tables.isFetchingNextPage}
                        onLoad={() => {
                            void tables.fetchNextPage();
                        }}
                    />
                </>
            )}
        </section>
    );
}
