import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { Table2Icon } from "lucide-react";
import { useDeferredValue, useEffect, useMemo, useRef, useState } from "react";

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
import {
    aliasName,
    count,
    date,
    exactBytes,
    exactCount,
    number,
    shortId,
    timestamp,
} from "../format";
import { buttonVariants } from "../ui/Button";
import { Select } from "../ui/Select";
import { namespaceKey, sortColumn, type TablesSearch } from "./tables/search";

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
        location: [catalogs.join(", "), namespaces.join(", ")]
            .filter((value) => value !== "")
            .join(", "),
    };
}

/** Canonical tables the session may see, named through their aliases. */
export function TablesPage() {
    const search = useSearch({ from: "/tables" });
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

    const namespaceOptions = useMemo(() => {
        const options = new Map<string, string[]>();
        for (const table of items) {
            for (const alias of table.aliases) {
                options.set(namespaceKey(alias.namespace), alias.namespace);
            }
        }
        return [...options.entries()]
            .map(([value, levels]) => ({ value, levels }))
            .sort((left, right) =>
                left.levels
                    .join(".")
                    .localeCompare(right.levels.join("."), undefined, {
                        numeric: true,
                    }),
            );
    }, [items]);
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
                    (alias) =>
                        namespaceKey(alias.namespace) === search.namespace,
                )
            ) {
                return false;
            }
            if (needle === "") {
                return true;
            }
            const info = display(table, names);
            return [
                info.name,
                info.catalogs,
                info.namespaces,
                table.id,
                table.table_uuid,
                ...table.aliases.map((alias) => aliasName(alias, names)),
            ]
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

    const tableFrame = useRef<HTMLDivElement>(null);
    const [tableOverflows, setTableOverflows] = useState(false);
    useEffect(() => {
        const container = tableFrame.current?.querySelector<HTMLElement>(
            '[data-slot="table-container"]',
        );
        if (container === null || container === undefined) {
            setTableOverflows(false);
            return undefined;
        }
        const measure = () => {
            setTableOverflows(
                container.scrollWidth > container.clientWidth + 1,
            );
        };
        measure();
        if (typeof ResizeObserver === "undefined") {
            return undefined;
        }
        const observer = new ResizeObserver(measure);
        observer.observe(container);
        const table = container.querySelector("table");
        if (table !== null) {
            observer.observe(table);
        }
        return () => {
            observer.disconnect();
        };
    }, [visible.length]);

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
                                {info.location !== "" && (
                                    <span className="sr-only">
                                        {" "}
                                        in {info.location}
                                    </span>
                                )}
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
            id: "records",
            header: "Rows",
            align: "end",
            // Sorting compares doubles, while the cell keeps every digit, so
            // only an ordering tie above 2^53 could differ.
            sortValue: (table) => Number(table.statistics?.record_count ?? -1),
            cell: (table) => (
                <span className="tabular-nums">
                    {table.statistics === null || table.statistics === undefined
                        ? "—"
                        : exactCount(table.statistics.record_count)}
                </span>
            ),
        },
        {
            id: "size",
            header: "Data size",
            align: "end",
            sortValue: (table) => Number(table.statistics?.data_bytes ?? -1),
            cell: (table) => (
                <span className="tabular-nums">
                    {table.statistics === null || table.statistics === undefined
                        ? "—"
                        : exactBytes(table.statistics.data_bytes)}
                </span>
            ),
        },
        {
            id: "files",
            header: "Data files",
            align: "end",
            sortValue: (table) => Number(table.statistics?.data_files ?? -1),
            cell: (table) => (
                <span className="tabular-nums">
                    {table.statistics === null || table.statistics === undefined
                        ? "—"
                        : exactCount(table.statistics.data_files)}
                </span>
            ),
        },
        {
            id: "small",
            header: "Small files",
            align: "end",
            sortValue: (table) => Number(table.statistics?.small_files ?? -1),
            cell: (table) => (
                <span className="tabular-nums">
                    {table.statistics === null || table.statistics === undefined
                        ? "—"
                        : exactCount(table.statistics.small_files)}
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
                                    className="text-sm font-medium whitespace-nowrap text-foreground"
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
                                    className="text-sm font-medium whitespace-nowrap text-foreground"
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
                                                ({ value, levels }) => ({
                                                    value,
                                                    label: levels.join("."),
                                                }),
                                            ),
                                        ]}
                                    />
                                </span>
                            </span>
                        )}
                    </FilterBar>
                    {(filtersActive || search.sort !== undefined) &&
                        tables.hasNextPage && (
                            <p className="mb-3 text-xs text-muted-foreground">
                                Search and sort cover the{" "}
                                {count(items.length, "loaded table")}. Load more
                                to widen them.
                            </p>
                        )}
                    <div ref={tableFrame}>
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
                                    No tables match these filters. Adjust the
                                    search or clear the filters to see every
                                    loaded table.
                                </NoResults>
                            }
                        />
                    </div>
                    {tableOverflows && (
                        <p className="mt-2 text-xs text-muted-foreground">
                            Scroll horizontally for more columns.
                        </p>
                    )}
                    {(visible.length > 0 ||
                        tables.hasNextPage ||
                        !filtersActive) && (
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
