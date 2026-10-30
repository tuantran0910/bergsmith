// The user directory (ADR 0067): provisioned people, searched by display
// name and filtered by activity on the coordinator.

import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { useMemo, type ReactNode } from "react";

import type { Schemas } from "../../api/client";
import { useUserFacets, useUserList } from "../../api/queries";
import { DataGrid } from "../../components/DataGrid";
import type { DataTableColumn } from "../../components/DataTable";
import { ErrorNotice } from "../../components/ErrorNotice";
import {
    FacetFilter,
    facetOptions,
    FilterPills,
    type FilterPill,
} from "../../components/FacetFilter";
import { FilterBar } from "../../components/FilterBar";
import {
    flagParam,
    SEARCH_DELAY_MS,
    searchText,
} from "../../components/listSearch";
import { ListTools } from "../../components/ListTools";
import {
    LoadMore,
    LoadingDetail,
    NoResults,
    PageHeader,
} from "../../components/Page";
import { useColumnVisibility } from "../../components/useColumnVisibility";
import { type UsersSearch, validateUsersSearch } from "./search";

/** The parameters a saved view keeps. */
const VIEW_KEYS = ["q", "active"];

function activeLabel(value: string): string {
    return value === "true" ? "Active" : "Deactivated";
}

/** Users of the organization, with `provision` as the page action. */
export function Users({ provision }: { provision: ReactNode }) {
    const search = useSearch({ from: "/users" });
    const navigate = useNavigate();
    const filter = { active: search.active, q: search.q };
    const users = useUserList(filter);
    const items = useMemo(
        () => users.data?.pages.flatMap((page) => page.items) ?? [],
        [users.data],
    );
    const filtersActive = search.q !== undefined || search.active !== undefined;
    const facets = useUserFacets(filter, filtersActive || items.length > 0);
    const [visibility, setVisibility] = useColumnVisibility("users");

    /** Applies `patch`; an `undefined` value removes that parameter. */
    function change(patch: {
        [K in keyof UsersSearch]?: UsersSearch[K] | undefined;
    }): void {
        const merged = { ...search, ...patch };
        const q = searchText(merged.q);
        const active = flagParam(merged.active);
        void navigate({
            to: "/users",
            search: {
                ...(q !== undefined ? { q } : {}),
                ...(active !== undefined ? { active } : {}),
            },
            replace: true,
        });
    }

    function clearFilters(): void {
        change({ q: undefined, active: undefined });
    }

    const columns = useMemo<DataTableColumn<Schemas["Principal"]>[]>(
        () => [
            {
                id: "name",
                header: "Name",
                cell: (principal) => (
                    <Link
                        to="/users/$principalId"
                        params={{ principalId: principal.id }}
                        className="break-words"
                    >
                        {principal.display_name}
                    </Link>
                ),
            },
            {
                id: "status",
                header: "Status",
                cell: (principal) => (
                    <span className="text-muted-foreground">
                        {principal.active ? "Active" : "Deactivated"}
                    </span>
                ),
            },
        ],
        [],
    );

    const pills: FilterPill[] =
        search.active === undefined
            ? []
            : [
                  {
                      key: "active",
                      label: "Status",
                      value: activeLabel(search.active),
                      onRemove: () => {
                          change({ active: undefined });
                      },
                  },
              ];

    return (
        <section>
            <PageHeader
                title="Users"
                description="Provisioned users and their access to this organization."
                actions={provision}
            />
            {users.isPending && <LoadingDetail label="Loading principals…" />}
            {users.error && (
                <ErrorNotice
                    error={users.error}
                    onRetry={() => {
                        void users.refetch();
                    }}
                />
            )}
            {users.isSuccess && items.length === 0 && !filtersActive && (
                <p>No provisioned users.</p>
            )}
            {(filtersActive || items.length > 0) && (
                <>
                    <FilterBar
                        query={search.q ?? ""}
                        onQueryChange={(value) => {
                            change({ q: value });
                        }}
                        delay={SEARCH_DELAY_MS}
                        queryLabel="Search users"
                        placeholder="Search by name"
                        // An empty result's notice owns the clear action.
                        onClear={
                            filtersActive && items.length > 0
                                ? clearFilters
                                : undefined
                        }
                        tools={
                            <ListTools
                                page="principals"
                                search={search}
                                keys={VIEW_KEYS}
                                columns={columns}
                                visibility={visibility}
                                onVisibilityChange={setVisibility}
                                onApply={(params) => {
                                    void navigate({
                                        to: "/users",
                                        search: validateUsersSearch(params),
                                        replace: true,
                                    });
                                }}
                            />
                        }
                        pills={<FilterPills pills={pills} />}
                    >
                        <FacetFilter
                            label="Status"
                            allLabel="All users"
                            options={facetOptions(
                                facets.data?.active,
                                search.active === undefined
                                    ? []
                                    : [search.active],
                                activeLabel,
                            )}
                            selected={
                                search.active === undefined
                                    ? []
                                    : [search.active]
                            }
                            onChange={(values) => {
                                change({ active: flagParam(values[0]) });
                            }}
                        />
                    </FilterBar>
                    {users.isSuccess && (
                        <>
                            <DataGrid
                                rows={items}
                                columns={columns}
                                getRowKey={(principal) => principal.id}
                                columnVisibility={visibility}
                                caption="Users"
                                refreshing={users.isPlaceholderData}
                                empty={
                                    <NoResults onClear={clearFilters}>
                                        No users match these filters.
                                    </NoResults>
                                }
                            />
                            {items.length > 0 && (
                                <LoadMore
                                    shown={items.length}
                                    noun="users"
                                    hasMore={users.hasNextPage}
                                    loading={users.isFetchingNextPage}
                                    onLoad={() => {
                                        void users.fetchNextPage();
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
