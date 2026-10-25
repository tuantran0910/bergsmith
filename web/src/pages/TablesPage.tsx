import { Link, useNavigate } from "@tanstack/react-router";
import { Table2Icon } from "lucide-react";
import { useDeferredValue, useMemo } from "react";

import type { Schemas } from "../api/client";
import { useCatalogNames, useTables } from "../api/queries";
import {
    DataTable,
    type DataTableColumn,
    type SortState,
} from "../components/DataTable";
import { ErrorNotice } from "../components/ErrorNotice";
import { FilterBar } from "../components/FilterBar";
import {
    EmptyState,
    LoadMore,
    LoadingRows,
    NoResults,
    PageHeader,
} from "../components/Page";
import { ResourceLabel } from "../components/ResourceLabel";
import { aliasName, date, number, shortId, timestamp } from "../format";
import { buttonVariants } from "../ui/Button";
import { Select } from "../ui/Select";
import { sortColumn, type TablesSearch } from "./tables/search";

/** The names and context one table shows, without losing its other aliases. */
function display(table: Schemas["Table"], names: ReadonlyMap<string, string>) {
    const [primary, ...others] = table.aliases;
    const catalogs = [
        ...new Set(
            table.aliases.map(
                (alias) => names.get(alias.catalog_id) ?? alias.catalog_id,
            ),
        ),
    ];
    const namespaces = [
        ...new Set(table.aliases.map((alias) => alias.namespace.join("."))),
    ];
    return {
        name: primary?.name ?? shortId(table.id),
        fullName: primary === undefined ? undefined : aliasName(primary, names),
        context:
            others.length > 0
                ? `Also ${others.map((alias) => aliasName(alias, names)).join(", ")}`
                : primary === undefined
                  ? "No registered names"
                  : undefined,
        catalogs: catalogs.length > 0 ? catalogs.join(", ") : "None",
        namespaces: namespaces.length > 0 ? namespaces.join(", ") : "None",
    };
}

/** Canonical tables the session may see, named through their aliases. */
export function TablesPage({ search }: { search: TablesSearch }) {
    const navigate = useNavigate();
    const tables = useTables();
    const catalogs = useCatalogNames();
    const names = useMemo(
        () => catalogs.data ?? new Map<string, string>(),
        [catalogs.data],
    );
    const items = useMemo(
        () => tables.data?.pages.flatMap((page) => page.items) ?? [],
        [tables.data],
    );

    const namespaceOptions = useMemo(
        () =>
            [
                ...new Set(
                    items.flatMap((table) =>
                        table.aliases.map((alias) => alias.namespace.join(".")),
                    ),
                ),
            ].sort((left, right) => left.localeCompare(right)),
        [items],
    );
    const catalogOptions = useMemo(
        () =>
            [...names.entries()].sort(([, left], [, right]) =>
                left.localeCompare(right),
            ),
        [names],
    );

    const deferredQuery = useDeferredValue(search.q ?? "");
    const visible = useMemo(() => {
        const needle = deferredQuery.trim().toLowerCase();
        return items.filter((table) => {
            if (
                search.catalog !== undefined &&
                !table.aliases.some(
                    (alias) => alias.catalog_id === search.catalog,
                )
            ) {
                return false;
            }
            if (
                search.namespace !== undefined &&
                !table.aliases.some(
                    (alias) => alias.namespace.join(".") === search.namespace,
                )
            ) {
                return false;
            }
            if (needle === "") {
                return true;
            }
            return table.aliases
                .map((alias) => aliasName(alias, names))
                .join(" ")
                .toLowerCase()
                .includes(needle);
        });
    }, [items, deferredQuery, search.catalog, search.namespace, names]);

    const filtersActive =
        (search.q ?? "") !== "" ||
        search.catalog !== undefined ||
        search.namespace !== undefined;

    function change(patch: Partial<TablesSearch>): void {
        const merged = { ...search, ...patch };
        const next: TablesSearch = {
            ...(merged.q !== undefined && merged.q !== ""
                ? { q: merged.q }
                : {}),
            ...(merged.catalog !== undefined && merged.catalog !== ""
                ? { catalog: merged.catalog }
                : {}),
            ...(merged.namespace !== undefined && merged.namespace !== ""
                ? { namespace: merged.namespace }
                : {}),
            ...(merged.sort !== undefined ? { sort: merged.sort } : {}),
            ...(merged.order !== undefined ? { order: merged.order } : {}),
        };
        void navigate({ to: "/tables", search: next, replace: true });
    }

    function clearFilters(): void {
        change({ q: "", catalog: "", namespace: "" });
    }

    function onSortChange(next: SortState): void {
        const column = sortColumn(next.column);
        if (column !== undefined) {
            change({ sort: column, order: next.direction });
        }
    }

    const sort: SortState = {
        column: search.sort ?? "name",
        direction: search.order ?? "asc",
    };

    const columns: DataTableColumn<Schemas["Table"]>[] = [
        {
            id: "name",
            header: "Table",
            sortValue: (table) => display(table, names).name,
            cell: (table) => {
                const info = display(table, names);
                return (
                    <ResourceLabel
                        name={
                            <Link
                                to="/tables/$tableId"
                                params={{ tableId: table.id }}
                                title={info.fullName ?? table.id}
                                className="break-words"
                            >
                                {info.name}
                            </Link>
                        }
                        context={info.context}
                        copy={{
                            value: table.table_uuid,
                            label: "table UUID",
                        }}
                        details={
                            <span>
                                Registered names:{" "}
                                {table.aliases.length === 0
                                    ? "None"
                                    : table.aliases
                                          .map((alias) =>
                                              aliasName(alias, names),
                                          )
                                          .join(", ")}
                            </span>
                        }
                    />
                );
            },
        },
        {
            id: "catalog",
            header: "Catalog",
            sortValue: (table) => display(table, names).catalogs,
            cell: (table) => (
                <span className="text-muted-foreground">
                    {display(table, names).catalogs}
                </span>
            ),
        },
        {
            id: "namespace",
            header: "Namespace",
            sortValue: (table) => display(table, names).namespaces,
            cell: (table) => (
                <span className="text-muted-foreground">
                    {display(table, names).namespaces}
                </span>
            ),
        },
        {
            id: "registered",
            header: "Registered",
            sortValue: (table) => table.created_at,
            cell: (table) => (
                <time
                    dateTime={table.created_at}
                    title={timestamp(table.created_at)}
                    className="whitespace-nowrap text-muted-foreground"
                >
                    {date(table.created_at)}
                </time>
            ),
        },
    ];

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
                    action={
                        <Link
                            to="/catalogs"
                            className={buttonVariants({
                                variant: "outline",
                                size: "sm",
                                className: "hover:no-underline",
                            })}
                        >
                            Browse catalogs
                        </Link>
                    }
                >
                    A table appears here once its catalog is connected and you
                    hold a grant on the catalog or the table.
                </EmptyState>
            )}
            {items.length > 0 && (
                <>
                    <FilterBar
                        query={search.q ?? ""}
                        onQueryChange={(value) => {
                            change({ q: value });
                        }}
                        queryLabel="Search tables"
                        placeholder="Search by table or namespace"
                        onClear={
                            filtersActive && visible.length > 0
                                ? clearFilters
                                : undefined
                        }
                        result={
                            filtersActive
                                ? `${number(visible.length)} of ${number(items.length)} ${
                                      tables.hasNextPage ? "loaded" : "tables"
                                  }`
                                : undefined
                        }
                    >
                        {catalogOptions.length > 0 && (
                            <span className="flex items-center gap-1.5">
                                <label
                                    htmlFor="table-catalog"
                                    className="text-xs text-muted-foreground"
                                >
                                    Catalog
                                </label>
                                <span className="w-36">
                                    <Select
                                        id="table-catalog"
                                        value={search.catalog ?? ""}
                                        onValueChange={(value) => {
                                            change({ catalog: value });
                                        }}
                                        options={[
                                            {
                                                value: "",
                                                label: "All catalogs",
                                            },
                                            ...catalogOptions.map(
                                                ([id, label]) => ({
                                                    value: id,
                                                    label,
                                                }),
                                            ),
                                        ]}
                                    />
                                </span>
                            </span>
                        )}
                        {namespaceOptions.length > 0 && (
                            <span className="flex items-center gap-1.5">
                                <label
                                    htmlFor="table-namespace"
                                    className="text-xs text-muted-foreground"
                                >
                                    Namespace
                                </label>
                                <span className="w-36">
                                    <Select
                                        id="table-namespace"
                                        value={search.namespace ?? ""}
                                        onValueChange={(value) => {
                                            change({ namespace: value });
                                        }}
                                        options={[
                                            {
                                                value: "",
                                                label: "All namespaces",
                                            },
                                            ...namespaceOptions.map(
                                                (namespace) => ({
                                                    value: namespace,
                                                    label: namespace,
                                                }),
                                            ),
                                        ]}
                                    />
                                </span>
                            </span>
                        )}
                    </FilterBar>
                    <DataTable
                        rows={visible}
                        columns={columns}
                        getRowKey={(table) => table.id}
                        sort={sort}
                        onSortChange={onSortChange}
                        caption="Tables"
                        empty={
                            <NoResults
                                onClear={
                                    filtersActive ? clearFilters : undefined
                                }
                            >
                                No tables match these filters. Adjust the search
                                or clear the filters to see every loaded table.
                            </NoResults>
                        }
                    />
                    <p className="mt-2 text-xs text-muted-foreground md:hidden">
                        Scroll horizontally for more columns.
                    </p>
                    {(visible.length > 0 || !filtersActive) && (
                        <LoadMore
                            shown={visible.length}
                            noun="tables"
                            hasMore={tables.hasNextPage}
                            loading={tables.isFetchingNextPage}
                            onLoad={() => {
                                void tables.fetchNextPage();
                            }}
                        />
                    )}
                </>
            )}
        </section>
    );
}
