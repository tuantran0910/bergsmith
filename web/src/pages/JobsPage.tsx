import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { ListChecksIcon } from "lucide-react";
import { useMemo } from "react";

import type { Job } from "../api/client";
import { useCatalogNames, useJobFacets, useJobs } from "../api/queries";
import { ColumnsMenu } from "../components/ColumnsMenu";
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
import {
    joinValues,
    SEARCH_DELAY_MS,
    searchText,
    valueList,
} from "../components/listSearch";
import {
    EmptyState,
    LoadMore,
    LoadingRows,
    NoResults,
    PageHeader,
} from "../components/Page";
import { ResourceLabel } from "../components/ResourceLabel";
import { CopyButton } from "../components/ResourceId";
import { StatusBadge } from "../components/StatusBadge";
import { useColumnVisibility } from "../components/useColumnVisibility";
import { shortId, tableName, timestamp, words } from "../format";
import {
    executionStatus,
    operationLabel,
    publicationStatus,
} from "../jobs/status";
import { JOB_OPERATIONS, JOB_STATES, type JobsSearch } from "./jobs/search";

function stateLabel(state: string): string {
    return state === "NoOp" ? "No-op" : words(state);
}

function operationName(operation: string): string {
    const known = JOB_OPERATIONS.find((candidate) => candidate === operation);
    return known === undefined ? operation : operationLabel(known);
}

/**
 * Jobs the session may see, with execution and publication side by side.
 * The coordinator filters, searches, and orders them (ADR 0067), so every
 * filter covers the whole list rather than the loaded pages.
 */
export function JobsPage() {
    const search = useSearch({ from: "/jobs" });
    const navigate = useNavigate();
    const states = valueList(search.state, JOB_STATES);
    const operations = valueList(search.operation, JOB_OPERATIONS);
    const filter = {
        state: joinValues(states),
        operation: joinValues(operations),
        q: search.q,
    };
    const order = search.order ?? "newest";
    const jobs = useJobs(filter, order);

    const catalogs = useCatalogNames();
    const names = useMemo(
        () => catalogs.data ?? new Map<string, string>(),
        [catalogs.data],
    );
    const items = useMemo(
        () => jobs.data?.pages.flatMap((page) => page.items) ?? [],
        [jobs.data],
    );
    const [visibility, setVisibility] = useColumnVisibility("jobs");
    const filtersActive =
        states.length > 0 || operations.length > 0 || search.q !== undefined;
    // Counts serve the filter controls, which show once there is a list.
    const facets = useJobFacets(filter, filtersActive || items.length > 0);

    /** Applies `patch`; an `undefined` value removes that parameter. */
    function change(patch: {
        [K in keyof JobsSearch]?: JobsSearch[K] | undefined;
    }): void {
        const merged = { ...search, ...patch };
        const q = searchText(merged.q);
        const next: JobsSearch = {
            ...(q !== undefined ? { q } : {}),
            ...(merged.state !== undefined ? { state: merged.state } : {}),
            ...(merged.operation !== undefined
                ? { operation: merged.operation }
                : {}),
            ...(merged.order !== undefined ? { order: merged.order } : {}),
        };
        void navigate({ to: "/jobs", search: next, replace: true });
    }

    function clearFilters(): void {
        change({ q: undefined, state: undefined, operation: undefined });
    }

    const columns = useMemo<DataTableColumn<Job>[]>(
        () => [
            {
                id: "table",
                header: "Table",
                className: "min-w-52",
                cell: (job) => (
                    <ResourceLabel
                        name={
                            <Link
                                to="/tables/$tableId"
                                params={{ tableId: job.table_id }}
                                title={job.table_id}
                            >
                                {tableName(
                                    job.table_aliases,
                                    job.table_id,
                                    names,
                                )}
                            </Link>
                        }
                        context={
                            <span className="inline-flex items-center gap-1">
                                <Link
                                    to="/jobs/$jobId"
                                    params={{ jobId: job.id }}
                                    title={job.id}
                                    className="hover:text-foreground"
                                >
                                    Job {shortId(job.id)}
                                </Link>
                                <CopyButton value={job.id} label="job ID" />
                            </span>
                        }
                    />
                ),
            },
            {
                id: "execution",
                header: "Execution",
                cell: (job) => <StatusBadge status={executionStatus(job)} />,
            },
            {
                id: "publication",
                header: "Publication",
                cell: (job) => <StatusBadge status={publicationStatus(job)} />,
            },
            {
                id: "operation",
                header: "Operation",
                cell: (job) => (
                    <span className="text-muted-foreground">
                        {operationLabel(job.operation)}
                    </span>
                ),
            },
            {
                id: "state",
                header: "State",
                cell: (job) => (
                    <span className="text-muted-foreground">{job.state}</span>
                ),
            },
            {
                id: "requester",
                header: "Requested by",
                cell: (job) => (
                    <span className="text-muted-foreground">
                        {job.created_by_name}
                    </span>
                ),
            },
            {
                id: "created",
                header: "Created",
                serverSort: true,
                cell: (job) => (
                    <time
                        dateTime={job.created_at}
                        className="whitespace-nowrap text-muted-foreground"
                    >
                        {timestamp(job.created_at)}
                    </time>
                ),
            },
        ],
        [names],
    );

    const sort: SortState = {
        column: "created",
        direction: order === "newest" ? "desc" : "asc",
    };

    const pills: FilterPill[] = [
        ...states.map((state) => ({
            key: `state:${state}`,
            label: "State",
            value: stateLabel(state),
            onRemove: () => {
                change({
                    state: joinValues(states.filter((item) => item !== state)),
                });
            },
        })),
        ...operations.map((operation) => ({
            key: `operation:${operation}`,
            label: "Operation",
            value: operationName(operation),
            onRemove: () => {
                change({
                    operation: joinValues(
                        operations.filter((item) => item !== operation),
                    ),
                });
            },
        })),
    ];

    const unfiltered = jobs.isSuccess && items.length === 0 && !filtersActive;

    return (
        <section>
            <PageHeader
                title="Jobs"
                description="Maintenance runs, newest first. Execution and publication are reported separately: only a confirmed publication changed the table."
            />
            {jobs.isPending && <LoadingRows label="Loading jobs…" />}
            {jobs.error && (
                <ErrorNotice
                    error={jobs.error}
                    onRetry={() => {
                        void jobs.refetch();
                    }}
                />
            )}
            {unfiltered && (
                <EmptyState
                    icon={ListChecksIcon}
                    title="No jobs are visible to you."
                >
                    Run maintenance from a table's page to start one.
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
                        queryLabel="Search jobs"
                        placeholder="Search by table name"
                        // An empty result's notice owns the clear action.
                        onClear={
                            filtersActive && items.length > 0
                                ? clearFilters
                                : undefined
                        }
                        tools={
                            <ColumnsMenu
                                columns={columns}
                                visibility={visibility}
                                onChange={setVisibility}
                            />
                        }
                        pills={<FilterPills pills={pills} />}
                    >
                        <FacetFilter
                            label="State"
                            multiple
                            options={facetOptions(
                                facets.data?.state,
                                states,
                                stateLabel,
                            )}
                            selected={states}
                            onChange={(values) => {
                                change({
                                    state: joinValues(
                                        valueList(values.join(","), JOB_STATES),
                                    ),
                                });
                            }}
                        />
                        <FacetFilter
                            label="Operation"
                            multiple
                            options={facetOptions(
                                facets.data?.operation,
                                operations,
                                operationName,
                            )}
                            selected={operations}
                            onChange={(values) => {
                                change({
                                    operation: joinValues(
                                        valueList(
                                            values.join(","),
                                            JOB_OPERATIONS,
                                        ),
                                    ),
                                });
                            }}
                        />
                    </FilterBar>
                    {jobs.isSuccess && (
                        <>
                            <DataGrid
                                rows={items}
                                columns={columns}
                                getRowKey={(job) => job.id}
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
                                caption="Jobs"
                                refreshing={jobs.isPlaceholderData}
                                empty={
                                    <NoResults onClear={clearFilters}>
                                        No jobs match these filters. Adjust or
                                        clear them to see every job.
                                    </NoResults>
                                }
                            />
                            {items.length > 0 && (
                                <LoadMore
                                    shown={items.length}
                                    noun="jobs"
                                    hasMore={jobs.hasNextPage}
                                    loading={jobs.isFetchingNextPage}
                                    onLoad={() => {
                                        void jobs.fetchNextPage();
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
