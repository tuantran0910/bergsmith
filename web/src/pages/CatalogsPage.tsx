import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { DatabaseIcon, PlusIcon } from "lucide-react";
import { useMemo } from "react";

import { canManageCatalog } from "../api/auth";
import type { Schemas } from "../api/client";
import { useCatalogFacets, useCatalogList } from "../api/queries";
import { DataGrid } from "../components/DataGrid";
import type { DataTableColumn } from "../components/DataTable";
import { ErrorNotice } from "../components/ErrorNotice";
import {
    FacetFilter,
    facetOptions,
    FilterPills,
    type FilterPill,
} from "../components/FacetFilter";
import { FilterBar } from "../components/FilterBar";
import {
    flagParam,
    SEARCH_DELAY_MS,
    searchText,
} from "../components/listSearch";
import { ListTools } from "../components/ListTools";
import {
    EmptyState,
    LoadMore,
    LoadingRows,
    NoResults,
    PageHeader,
} from "../components/Page";
import { useColumnVisibility } from "../components/useColumnVisibility";
import { useSession } from "../queryClient";
import { Badge } from "../ui/Badge";
import { buttonVariants } from "../ui/Button";
import { type CatalogsSearch, validateCatalogsSearch } from "./catalogs/search";

/** The parameters a saved view keeps. */
const VIEW_KEYS = ["q", "enabled"];

function enabledLabel(value: string): string {
    return value === "true" ? "Enabled" : "Disabled";
}

/**
 * Authorized connections, including paused and disabled connections. The
 * coordinator searches names and filters by state (ADR 0067).
 */
export function CatalogsPage() {
    const search = useSearch({ from: "/catalogs" });
    const navigate = useNavigate();
    const session = useSession();
    const filter = { enabled: search.enabled, q: search.q };
    const catalogs = useCatalogList(filter);
    const items = useMemo(
        () => catalogs.data?.pages.flatMap((page) => page.items) ?? [],
        [catalogs.data],
    );
    const filtersActive =
        search.q !== undefined || search.enabled !== undefined;
    const facets = useCatalogFacets(filter, filtersActive || items.length > 0);
    const [visibility, setVisibility] = useColumnVisibility("catalogs");

    /** Applies `patch`; an `undefined` value removes that parameter. */
    function change(patch: {
        [K in keyof CatalogsSearch]?: CatalogsSearch[K] | undefined;
    }): void {
        const merged = { ...search, ...patch };
        const q = searchText(merged.q);
        const enabled = flagParam(merged.enabled);
        void navigate({
            to: "/catalogs",
            search: {
                ...(q !== undefined ? { q } : {}),
                ...(enabled !== undefined ? { enabled } : {}),
            },
            replace: true,
        });
    }

    function clearFilters(): void {
        change({ q: undefined, enabled: undefined });
    }

    const columns = useMemo<DataTableColumn<Schemas["Catalog"]>[]>(
        () => [
            {
                id: "name",
                header: "Catalog",
                cell: (catalog) => (
                    <Link
                        to="/catalogs/$catalogId"
                        params={{ catalogId: catalog.id }}
                        className="font-semibold break-words"
                    >
                        {catalog.name}
                    </Link>
                ),
            },
            {
                id: "status",
                header: "Status",
                cell: (catalog) => (
                    <Badge>{catalog.enabled ? "Enabled" : "Disabled"}</Badge>
                ),
            },
            {
                id: "maintenance",
                header: "Maintenance",
                cell: (catalog) => (
                    <span className="text-muted-foreground">
                        {catalog.settings.maintenance_enabled ? "On" : "Paused"}
                    </span>
                ),
            },
            {
                id: "budget",
                header: "Execution budget",
                align: "end",
                cell: (catalog) => (
                    <span className="tabular-nums">
                        {catalog.settings.max_execution_seconds} s
                    </span>
                ),
            },
        ],
        [],
    );

    const pills: FilterPill[] =
        search.enabled === undefined
            ? []
            : [
                  {
                      key: "enabled",
                      label: "Status",
                      value: enabledLabel(search.enabled),
                      onRemove: () => {
                          change({ enabled: undefined });
                      },
                  },
              ];
    const unfiltered =
        catalogs.isSuccess && items.length === 0 && !filtersActive;

    return (
        <section>
            <PageHeader
                title="Catalogs"
                description="Connect Iceberg catalogs and choose the tables to maintain."
                actions={
                    session !== null &&
                    canManageCatalog(session) && (
                        <Link
                            to="/catalogs/new"
                            className={buttonVariants({
                                size: "lg",
                                className: "hover:no-underline",
                            })}
                        >
                            <PlusIcon aria-hidden="true" />
                            Add Connection
                        </Link>
                    )
                }
            />
            {catalogs.isPending && <LoadingRows label="Loading catalogs…" />}
            {catalogs.error && (
                <ErrorNotice
                    error={catalogs.error}
                    onRetry={() => {
                        void catalogs.refetch();
                    }}
                />
            )}
            {unfiltered && (
                <EmptyState
                    icon={DatabaseIcon}
                    title="No catalogs are visible to you."
                >
                    An administrator can add a connection or grant access to an
                    existing one.
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
                        queryLabel="Search catalogs"
                        placeholder="Search by name"
                        // An empty result's notice owns the clear action.
                        onClear={
                            filtersActive && items.length > 0
                                ? clearFilters
                                : undefined
                        }
                        tools={
                            <ListTools
                                page="catalogs"
                                search={search}
                                keys={VIEW_KEYS}
                                columns={columns}
                                visibility={visibility}
                                onVisibilityChange={setVisibility}
                                onApply={(params) => {
                                    void navigate({
                                        to: "/catalogs",
                                        search: validateCatalogsSearch(params),
                                        replace: true,
                                    });
                                }}
                            />
                        }
                        pills={<FilterPills pills={pills} />}
                    >
                        <FacetFilter
                            label="Status"
                            allLabel="All catalogs"
                            options={facetOptions(
                                facets.data?.enabled,
                                search.enabled === undefined
                                    ? []
                                    : [search.enabled],
                                enabledLabel,
                            )}
                            selected={
                                search.enabled === undefined
                                    ? []
                                    : [search.enabled]
                            }
                            onChange={(values) => {
                                change({ enabled: flagParam(values[0]) });
                            }}
                        />
                    </FilterBar>
                    {catalogs.isSuccess && (
                        <>
                            <DataGrid
                                rows={items}
                                columns={columns}
                                getRowKey={(catalog) => catalog.id}
                                columnVisibility={visibility}
                                caption="Catalogs"
                                refreshing={catalogs.isPlaceholderData}
                                empty={
                                    <NoResults onClear={clearFilters}>
                                        No catalogs match these filters.
                                    </NoResults>
                                }
                            />
                            {items.length > 0 && (
                                <LoadMore
                                    shown={items.length}
                                    noun="catalogs"
                                    hasMore={catalogs.hasNextPage}
                                    loading={catalogs.isFetchingNextPage}
                                    onLoad={() => {
                                        void catalogs.fetchNextPage();
                                    }}
                                />
                            )}
                        </>
                    )}
                </>
            )}
        </section>
    );
}
