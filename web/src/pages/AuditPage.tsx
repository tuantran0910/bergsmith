import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { ScrollTextIcon } from "lucide-react";
import { useMemo } from "react";

import { ApiFailure, type Schemas } from "../api/client";
import { useAuditFacets, useAuditList } from "../api/queries";
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
import { GrantRequired } from "../components/GrantRequired";
import {
    joinValues,
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
import { CopyButton } from "../components/ResourceId";
import { StatusBadge } from "../components/StatusBadge";
import { useColumnVisibility } from "../components/useColumnVisibility";
import { shortId, timestamp, words } from "../format";
import type { Tone } from "../jobs/status";
import {
    AUDIT_OUTCOMES,
    type AuditSearch,
    validateAuditSearch,
} from "./audit/search";

const OUTCOME_TONES: Record<Schemas["AuditEvent"]["outcome"], Tone> = {
    Allowed: "ok",
    Denied: "warning",
    Failed: "error",
};

/** The parameters a saved view keeps. */
const VIEW_KEYS = ["q", "outcome", "action", "resource_type", "actor", "order"];

/** A comma-separated parameter's values. */
function values(param: string | undefined): string[] {
    return param === undefined ? [] : param.split(",");
}

const DESCRIPTION =
    "Who did what in this organization, including refused requests.";

/** The actor by display name and kind, with the principal ID in the title and
 * a copy button; events without a principal show only the kind.
 */
function Actor({ event }: { event: Schemas["AuditEvent"] }) {
    const id = event.actor_principal_id;
    if (id === null) {
        return <span>{event.actor_kind}</span>;
    }
    return (
        <span className="inline-flex flex-col">
            <span className="inline-flex items-center gap-1">
                <span title={id} className="font-semibold">
                    {event.actor_display_name ?? shortId(id)}
                </span>
                <CopyButton value={id} label="principal ID" />
            </span>
            <span className="text-xs text-muted-foreground">
                {event.actor_kind}
            </span>
        </span>
    );
}

/**
 * The resource type leads; a short ID and copy button stay with it because
 * audit events do not carry a resource display name. Known resources link to
 * their detail pages.
 */
function Resource({ event }: { event: Schemas["AuditEvent"] }) {
    const id = event.resource_id;
    const type = words(event.resource_type);
    if (id === null) {
        return <span className="text-muted-foreground">{type}</span>;
    }
    const short = <code title={id}>{shortId(id)}</code>;
    const linked =
        event.resource_type === "job" ? (
            <Link
                to="/jobs/$jobId"
                params={{ jobId: id }}
                title={id}
                aria-label={`${type} ${shortId(id)}`}
            >
                {short}
            </Link>
        ) : event.resource_type === "catalog" ? (
            <Link
                to="/catalogs/$catalogId"
                params={{ catalogId: id }}
                title={id}
                aria-label={`${type} ${shortId(id)}`}
            >
                {short}
            </Link>
        ) : event.resource_type === "table" ? (
            <Link
                to="/tables/$tableId"
                params={{ tableId: id }}
                title={id}
                aria-label={`${type} ${shortId(id)}`}
            >
                {short}
            </Link>
        ) : (
            short
        );
    return (
        <span className="inline-flex flex-col">
            <span className="font-semibold">{type}</span>
            <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
                {linked}
                <CopyButton value={id} label={`${type.toLowerCase()} ID`} />
            </span>
            <span className="sr-only">
                {type} ID {id}
            </span>
        </span>
    );
}

/**
 * Organization audit history, newest first; requires organization-wide
 * `audit:read`. The coordinator filters, searches actor names, and orders
 * the records (ADR 0067).
 */
export function AuditPage() {
    const search = useSearch({ from: "/audit" });
    const navigate = useNavigate();
    const outcomes = values(search.outcome);
    const actions = values(search.action);
    const resourceTypes = values(search.resource_type);
    const filter = {
        outcome: search.outcome,
        action: search.action,
        resource_type: search.resource_type,
        actor: search.actor,
        q: search.q,
    };
    const order = search.order ?? "newest";
    const audit = useAuditList(filter, order);
    const items = useMemo(
        () => audit.data?.pages.flatMap((page) => page.items) ?? [],
        [audit.data],
    );
    const filtersActive =
        search.q !== undefined ||
        search.outcome !== undefined ||
        search.action !== undefined ||
        search.resource_type !== undefined ||
        search.actor !== undefined;
    const facets = useAuditFacets(filter, filtersActive || items.length > 0);
    const [visibility, setVisibility] = useColumnVisibility("audit");

    /** Applies `patch`; an `undefined` value removes that parameter. */
    function change(patch: {
        [K in keyof AuditSearch]?: AuditSearch[K] | undefined;
    }): void {
        const merged = { ...search, ...patch };
        void navigate({
            to: "/audit",
            search: validateAuditSearch({ ...merged, q: searchText(merged.q) }),
            replace: true,
        });
    }

    function clearFilters(): void {
        change({
            q: undefined,
            outcome: undefined,
            action: undefined,
            resource_type: undefined,
            actor: undefined,
        });
    }

    const columns = useMemo<DataTableColumn<Schemas["AuditEvent"]>[]>(
        () => [
            {
                id: "when",
                header: "When",
                serverSort: true,
                cell: (event) => (
                    <time
                        dateTime={event.occurred_at}
                        className="whitespace-nowrap text-muted-foreground"
                    >
                        {timestamp(event.occurred_at)}
                    </time>
                ),
            },
            {
                id: "actor",
                header: "Actor",
                cell: (event) => <Actor event={event} />,
            },
            {
                id: "action",
                header: "Action",
                cell: (event) => <code>{event.action}</code>,
            },
            {
                id: "resource",
                header: "Resource",
                cell: (event) => <Resource event={event} />,
            },
            {
                id: "outcome",
                header: "Outcome",
                cell: (event) => (
                    <StatusBadge
                        status={{
                            label: words(event.outcome),
                            tone: OUTCOME_TONES[event.outcome],
                        }}
                    />
                ),
            },
            {
                id: "detail",
                header: "Detail",
                cell: (event) => (
                    <code className="break-all text-muted-foreground">
                        {JSON.stringify(event.detail)}
                    </code>
                ),
            },
        ],
        [],
    );

    if (audit.error instanceof ApiFailure && audit.error.status === 403) {
        return (
            <section>
                <PageHeader title="Audit history" description={DESCRIPTION} />
                <GrantRequired>
                    Audit history requires an organization-wide grant with{" "}
                    <code>audit:read</code>.
                </GrantRequired>
            </section>
        );
    }

    const sort: SortState = {
        column: "when",
        direction: order === "newest" ? "desc" : "asc",
    };
    const pill = (
        label: string,
        key: "outcome" | "action" | "resource_type",
        selected: string[],
        name: (value: string) => string,
    ): FilterPill[] =>
        selected.map((value) => ({
            key: `${key}:${value}`,
            label,
            value: name(value),
            onRemove: () => {
                change({
                    [key]: joinValues(
                        selected.filter((item) => item !== value),
                    ),
                });
            },
        }));
    const pills: FilterPill[] = [
        ...pill("Outcome", "outcome", outcomes, words),
        ...pill("Action", "action", actions, (value) => value),
        ...pill("Resource", "resource_type", resourceTypes, words),
        ...(search.actor === undefined
            ? []
            : [
                  {
                      key: "actor",
                      label: "Actor",
                      value: shortId(search.actor),
                      onRemove: () => {
                          change({ actor: undefined });
                      },
                  },
              ]),
    ];
    const unfiltered = audit.isSuccess && items.length === 0 && !filtersActive;

    return (
        <section>
            <PageHeader title="Audit history" description={DESCRIPTION} />
            {audit.isPending && <LoadingRows label="Loading audit history…" />}
            {audit.error && (
                <ErrorNotice
                    error={audit.error}
                    onRetry={() => {
                        void audit.refetch();
                    }}
                />
            )}
            {unfiltered && (
                <EmptyState
                    icon={ScrollTextIcon}
                    title="No audit events yet."
                />
            )}
            {(filtersActive || items.length > 0) && (
                <>
                    <FilterBar
                        query={search.q ?? ""}
                        onQueryChange={(value) => {
                            change({ q: value });
                        }}
                        delay={SEARCH_DELAY_MS}
                        queryLabel="Search audit history"
                        placeholder="Search by actor name"
                        // An empty result's notice owns the clear action.
                        onClear={
                            filtersActive && items.length > 0
                                ? clearFilters
                                : undefined
                        }
                        tools={
                            <ListTools
                                page="audit"
                                search={search}
                                keys={VIEW_KEYS}
                                columns={columns}
                                visibility={visibility}
                                onVisibilityChange={setVisibility}
                                onApply={(params) => {
                                    void navigate({
                                        to: "/audit",
                                        search: validateAuditSearch(params),
                                        replace: true,
                                    });
                                }}
                            />
                        }
                        pills={<FilterPills pills={pills} />}
                    >
                        <FacetFilter
                            label="Outcome"
                            multiple
                            options={facetOptions(
                                facets.data?.outcome,
                                outcomes,
                                words,
                            )}
                            selected={outcomes}
                            onChange={(next) => {
                                change({
                                    outcome: joinValues(
                                        AUDIT_OUTCOMES.filter((value) =>
                                            next.includes(value),
                                        ),
                                    ),
                                });
                            }}
                        />
                        <FacetFilter
                            label="Action"
                            multiple
                            options={facetOptions(
                                facets.data?.action,
                                actions,
                                (value) => value,
                            )}
                            selected={actions}
                            onChange={(next) => {
                                change({ action: joinValues(next) });
                            }}
                        />
                        <FacetFilter
                            label="Resource"
                            multiple
                            options={facetOptions(
                                facets.data?.resource_type,
                                resourceTypes,
                                words,
                            )}
                            selected={resourceTypes}
                            onChange={(next) => {
                                change({ resource_type: joinValues(next) });
                            }}
                        />
                    </FilterBar>
                    {audit.isSuccess && (
                        <>
                            <DataGrid
                                rows={items}
                                columns={columns}
                                getRowKey={(event) => event.id}
                                sort={sort}
                                onSortChange={(next) => {
                                    change({
                                        order:
                                            next.direction === "asc"
                                                ? "oldest"
                                                : undefined,
                                    });
                                }}
                                columnVisibility={visibility}
                                caption="Audit history"
                                refreshing={audit.isPlaceholderData}
                                empty={
                                    <NoResults onClear={clearFilters}>
                                        No audit events match these filters.
                                    </NoResults>
                                }
                            />
                            {items.length > 0 && (
                                <LoadMore
                                    shown={items.length}
                                    noun="events"
                                    hasMore={audit.hasNextPage}
                                    loading={audit.isFetchingNextPage}
                                    onLoad={() => {
                                        void audit.fetchNextPage();
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
