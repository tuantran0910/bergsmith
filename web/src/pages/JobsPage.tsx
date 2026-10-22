import { Link } from "@tanstack/react-router";
import { ListChecksIcon } from "lucide-react";

import { useJobs } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import {
    EmptyState,
    GridFrame,
    LoadMore,
    LoadingRows,
    PageHeader,
} from "../components/Page";
import { StatusBadge } from "../components/StatusBadge";
import { shortId, timestamp } from "../format";
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
                                    <TableHead className="pl-4">Job</TableHead>
                                    <TableHead>Table</TableHead>
                                    <TableHead>Execution</TableHead>
                                    <TableHead>Publication</TableHead>
                                    <TableHead>State</TableHead>
                                    <TableHead className="pr-4">
                                        Created
                                    </TableHead>
                                </TableRow>
                            </TableHeader>
                            <TableBody>
                                {items.map((job) => (
                                    <TableRow key={job.id}>
                                        <TableCell className="pl-4">
                                            <Link
                                                to="/jobs/$jobId"
                                                params={{ jobId: job.id }}
                                                title={job.id}
                                                className="font-mono"
                                            >
                                                {shortId(job.id)}
                                            </Link>
                                        </TableCell>
                                        <TableCell>
                                            <Link
                                                to="/tables/$tableId"
                                                params={{
                                                    tableId: job.table_id,
                                                }}
                                                title={job.table_id}
                                                className="font-mono"
                                            >
                                                {shortId(job.table_id)}
                                            </Link>
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
                                            {job.state}
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
