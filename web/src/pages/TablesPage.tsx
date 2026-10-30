import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { PanelRightOpenIcon, Table2Icon } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { Schemas } from "../api/client";
import { useCatalogNames, useTableFacets, useTables } from "../api/queries";
import { DataGrid } from "../components/DataGrid";
import type { DataTableColumn, SortState } from "../components/DataTable";
import { ErrorNotice } from "../components/ErrorNotice";
import {
    FacetFilter,
    facetOptions,
    FilterPills,
    type FilterPill,
} from "../components/FacetFilter";
import { FilterBar } from "../components/FilterBar";
import { HealthBadge, type HealthState } from "../components/HealthBadge";
import { SEARCH_DELAY_MS, searchText } from "../components/listSearch";
import { ListTools } from "../components/ListTools";
import { useColumnVisibility } from "../components/useColumnVisibility";
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
    shortId,
    timestamp,
} from "../format";
import { Button, buttonVariants } from "../ui/Button";
import {
    healthFilter,
    sortColumn,
    type TablesSearch,
    validateTablesSearch,
} from "./tables/search";
import { TablePeek } from "./tables/TablePeek";

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

/** The parameters a saved view keeps. */
const VIEW_KEYS = ["q", "catalog", "health", "sort", "order"];

const HEALTH_LABELS: Record<string, string> = {
    critical: "Critical",
    warning: "Warning",
    healthy: "Healthy",
    unassessed: "Not assessed",
};

/** Critical first; an unassessed table sorts last, never as healthy. */
function healthRank(state: HealthState): number {
    switch (state) {
        case "critical":
            return 0;
        case "warning":
            return 1;
        case "healthy":
            return 2;
        default:
            return 3;
    }
}

/**
 * Canonical tables the session may see, named through their aliases. The
 * coordinator searches, filters, and orders by health across every visible
 * table (ADR 0059, ADR 0067); the other columns sort the loaded pages.
 */
export function TablesPage() {
    const search = useSearch({ from: "/tables" });
    const navigate = useNavigate();
    const filter = {
        health: healthFilter(search.health),
        catalog: search.catalog,
        q: search.q,
    };
    const tables = useTables({
        ...filter,
        sort: search.sort === "health" ? "health" : undefined,
    });

    const catalogs = useCatalogNames();
    const names = useMemo(
        () => catalogs.data ?? new Map<string, string>(),
        [catalogs.data],
    );
    const items = useMemo(
        () => tables.data?.pages.flatMap((page) => page.items) ?? [],
        [tables.data],
    );
    const [visibility, setVisibility] = useColumnVisibility("tables");

    const filtersActive =
        search.q !== undefined ||
        search.catalog !== undefined ||
        search.health !== undefined;
    // Counts serve the filter controls, which show once there is a list.
    const facets = useTableFacets(filter, filtersActive || items.length > 0);

    /** Applies `patch`; an `undefined` value removes that parameter. */
    function change(patch: {
        [K in keyof TablesSearch]?: TablesSearch[K] | undefined;
    }): void {
        const merged = { ...search, ...patch };
        const q = searchText(merged.q);
        const next: TablesSearch = {
            ...(q !== undefined ? { q } : {}),
            ...(merged.catalog !== undefined && merged.catalog !== ""
                ? { catalog: merged.catalog }
                : {}),
            ...(merged.health !== undefined ? { health: merged.health } : {}),
            ...(merged.sort !== undefined ? { sort: merged.sort } : {}),
            ...(merged.order !== undefined ? { order: merged.order } : {}),
            ...(merged.peek !== undefined ? { peek: merged.peek } : {}),
        };
        void navigate({ to: "/tables", search: next, replace: true });
    }

    // Stable, so the memoized columns keep it: it reads the address as it is
    // when the button is pressed.
    const openPeek = useCallback(
        (id: string) => {
            void navigate({
                from: "/tables",
                to: "/tables",
                search: (previous) => ({ ...previous, peek: id }),
                replace: true,
            });
        },
        [navigate],
    );

    function clearFilters(): void {
        change({ q: undefined, catalog: undefined, health: undefined });
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
    }, [items.length, visibility]);

    const columns = useMemo<DataTableColumn<Schemas["Table"]>[]>(
        () => [
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
                id: "health",
                header: "Health",
                sortValue: (table) => healthRank(table.health.state),
                cell: (table) => <HealthBadge state={table.health.state} />,
            },
            {
                id: "records",
                header: "Rows",
                align: "end",
                // Sorting compares doubles, while the cell keeps every digit, so
                // only an ordering tie above 2^53 could differ.
                sortValue: (table) =>
                    Number(table.statistics?.record_count ?? -1),
                cell: (table) => (
                    <span className="tabular-nums">
                        {table.statistics === null ||
                        table.statistics === undefined
                            ? "—"
                            : exactCount(table.statistics.record_count)}
                    </span>
                ),
            },
            {
                id: "size",
                header: "Data size",
                align: "end",
                sortValue: (table) =>
                    Number(table.statistics?.data_bytes ?? -1),
                cell: (table) => (
                    <span className="tabular-nums">
                        {table.statistics === null ||
                        table.statistics === undefined
                            ? "—"
                            : exactBytes(table.statistics.data_bytes)}
                    </span>
                ),
            },
            {
                id: "files",
                header: "Data files",
                align: "end",
                sortValue: (table) =>
                    Number(table.statistics?.data_files ?? -1),
                cell: (table) => (
                    <span className="tabular-nums">
                        {table.statistics === null ||
                        table.statistics === undefined
                            ? "—"
                            : exactCount(table.statistics.data_files)}
                    </span>
                ),
            },
            {
                id: "small",
                header: "Small files",
                align: "end",
                sortValue: (table) =>
                    Number(table.statistics?.small_files ?? -1),
                cell: (table) => (
                    <span className="tabular-nums">
                        {table.statistics === null ||
                        table.statistics === undefined
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
            {
                id: "preview",
                header: "",
                align: "end",
                hideable: false,
                card: "actions",
                cell: (table) => (
                    <Button
                        variant="ghost"
                        size="icon-sm"
                        aria-label={`Preview table ${display(table, names).name}`}
                        onClick={() => {
                            openPeek(table.id);
                        }}
                    >
                        <PanelRightOpenIcon aria-hidden="true" />
                    </Button>
                ),
            },
        ],
        [names, openPeek],
    );

    const catalogLabel = (id: string) => names.get(id) ?? shortId(id);
    const healthLabel = (value: string) => HEALTH_LABELS[value] ?? value;
    const pills: FilterPill[] = [
        ...(search.health === undefined
            ? []
            : [
                  {
                      key: "health",
                      label: "Health",
                      value: healthLabel(search.health),
                      onRemove: () => {
                          change({ health: undefined });
                      },
                  },
              ]),
        ...(search.catalog === undefined
            ? []
            : [
                  {
                      key: "catalog",
                      label: "Catalog",
                      value: catalogLabel(search.catalog),
                      onRemove: () => {
                          change({ catalog: undefined });
                      },
                  },
              ]),
    ];
    const unfiltered = tables.isSuccess && items.length === 0 && !filtersActive;

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
            {unfiltered && (
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
            {(filtersActive || items.length > 0) && (
                <>
                    <FilterBar
                        query={search.q ?? ""}
                        onQueryChange={(value) => {
                            change({ q: value });
                        }}
                        delay={SEARCH_DELAY_MS}
                        queryLabel="Search tables"
                        placeholder="Search by namespace or table name"
                        // An empty result's notice owns the clear action.
                        onClear={
                            filtersActive && items.length > 0
                                ? clearFilters
                                : undefined
                        }
                        tools={
                            <ListTools
                                page="tables"
                                search={search}
                                keys={VIEW_KEYS}
                                columns={columns}
                                visibility={visibility}
                                onVisibilityChange={setVisibility}
                                onApply={(params) => {
                                    void navigate({
                                        to: "/tables",
                                        search: validateTablesSearch(params),
                                        replace: true,
                                    });
                                }}
                            />
                        }
                        pills={<FilterPills pills={pills} />}
                    >
                        <FacetFilter
                            label="Health"
                            allLabel="All states"
                            options={facetOptions(
                                facets.data?.health,
                                search.health === undefined
                                    ? []
                                    : [search.health],
                                healthLabel,
                            )}
                            selected={
                                search.health === undefined
                                    ? []
                                    : [search.health]
                            }
                            onChange={(values) => {
                                change({ health: healthFilter(values[0]) });
                            }}
                        />
                        <FacetFilter
                            label="Catalog"
                            allLabel="All catalogs"
                            options={facetOptions(
                                facets.data?.catalog,
                                search.catalog === undefined
                                    ? []
                                    : [search.catalog],
                                catalogLabel,
                            )}
                            selected={
                                search.catalog === undefined
                                    ? []
                                    : [search.catalog]
                            }
                            onChange={(values) => {
                                change({ catalog: values[0] });
                            }}
                        />
                    </FilterBar>
                    {tables.isSuccess && (
                        <>
                            {search.sort !== undefined &&
                                search.sort !== "health" &&
                                tables.hasNextPage && (
                                    <p className="mb-3 text-xs text-muted-foreground">
                                        This order covers the{" "}
                                        {count(items.length, "loaded table")}.
                                        Load more to widen it.
                                    </p>
                                )}
                            <div ref={tableFrame}>
                                <DataGrid
                                    rows={items}
                                    columns={columns}
                                    getRowKey={(table) => table.id}
                                    sort={sort}
                                    onSortChange={onSortChange}
                                    columnVisibility={visibility}
                                    caption="Tables"
                                    refreshing={tables.isPlaceholderData}
                                    empty={
                                        <NoResults onClear={clearFilters}>
                                            No tables match these filters.
                                            Adjust the search or clear the
                                            filters to see every table.
                                        </NoResults>
                                    }
                                />
                            </div>
                            {tableOverflows && (
                                <p className="mt-2 text-xs text-muted-foreground">
                                    Scroll horizontally for more columns.
                                </p>
                            )}
                            {items.length > 0 && (
                                <LoadMore
                                    shown={items.length}
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
                </>
            )}
            {search.peek !== undefined && (
                <TablePeek
                    key={search.peek}
                    id={search.peek}
                    row={items.find((table) => table.id === search.peek)}
                    names={names}
                    onClose={() => {
                        change({ peek: undefined });
                    }}
                />
            )}
        </section>
    );
}
