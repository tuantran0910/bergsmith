import { Link } from "@tanstack/react-router";
import { ListChecksIcon } from "lucide-react";

import { useCatalogNames, useJobs } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import {
    EmptyState,
    GridFrame,
    LoadMore,
    LoadingRows,
    PageHeader,
} from "../components/Page";
import { ResourceLabel } from "../components/ResourceLabel";
import { CopyButton } from "../components/ResourceId";
import { StatusBadge } from "../components/StatusBadge";
import { shortId, tableName, timestamp } from "../format";
import { executionStatus, publicationStatus } from "../jobs/status";
import {
    Table,
    TableBody,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "../ui/Table";

/** Jobs the session may see, with execution and publication side by side. */
export function JobsPage() {
    const jobs = useJobs();
    const catalogs = useCatalogNames();
    const names = catalogs.data ?? new Map<string, string>();
    const items = jobs.data?.pages.flatMap((page) => page.items) ?? [];

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
            {jobs.isSuccess && items.length === 0 && (
                <EmptyState
                    icon={ListChecksIcon}
                    title="No jobs are visible to you."
                >
                    Run maintenance from a table's page to start one.
                </EmptyState>
            )}
            {items.length > 0 && (
                <>
                    <GridFrame>
                        <Table>
                            <TableHeader>
                                <TableRow>
                                    <TableHead className="min-w-52 pl-4">
                                        Table
                                    </TableHead>
                                    <TableHead>Execution</TableHead>
                                    <TableHead>Publication</TableHead>
                                    <TableHead>Operation</TableHead>
                                    <TableHead>State</TableHead>
                                    <TableHead>Requested by</TableHead>
                                    <TableHead className="pr-4">
                                        Created
                                    </TableHead>
                                </TableRow>
                            </TableHeader>
                            <TableBody>
                                {items.map((job) => (
                                    <TableRow key={job.id}>
                                        <TableCell className="min-w-52 pl-4">
                                            <ResourceLabel
                                                name={
                                                    <Link
                                                        to="/tables/$tableId"
                                                        params={{
                                                            tableId:
                                                                job.table_id,
                                                        }}
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
                                                            params={{
                                                                jobId: job.id,
                                                            }}
                                                            title={job.id}
                                                            className="hover:text-foreground"
                                                        >
                                                            Job{" "}
                                                            {shortId(job.id)}
                                                        </Link>
                                                        <CopyButton
                                                            value={job.id}
                                                            label="job ID"
                                                        />
                                                    </span>
                                                }
                                            />
                                        </TableCell>
                                        <TableCell>
                                            <StatusBadge
                                                status={executionStatus(job)}
                                            />
                                        </TableCell>
                                        <TableCell>
                                            <StatusBadge
                                                status={publicationStatus(job)}
                                            />
                                        </TableCell>
                                        <TableCell className="text-muted-foreground">
                                            {job.operation === "compact"
                                                ? "Compaction"
                                                : "Snapshot expiration"}
                                        </TableCell>
                                        <TableCell className="text-muted-foreground">
                                            {job.state}
                                        </TableCell>
                                        <TableCell className="text-muted-foreground">
                                            {job.created_by_name}
                                        </TableCell>
                                        <TableCell className="pr-4 whitespace-nowrap text-muted-foreground">
                                            {timestamp(job.created_at)}
                                        </TableCell>
                                    </TableRow>
                                ))}
                            </TableBody>
                        </Table>
                    </GridFrame>
                    <LoadMore
                        shown={items.length}
                        noun="jobs"
                        hasMore={jobs.hasNextPage}
                        loading={jobs.isFetchingNextPage}
                        onLoad={() => {
                            void jobs.fetchNextPage();
                        }}
                    />
                </>
            )}
        </section>
    );
}
