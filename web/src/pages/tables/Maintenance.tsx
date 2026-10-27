// The table detail's maintenance panel (BS-027.T2): what maintenance has done
// over the last thirty days, per terminal job and in totals.
//
// Impact comes from accepted work only: a retried task counts once, and the
// output of a job that a conflict discarded is reported separately rather
// than counted as a rewrite.

import { Link } from "@tanstack/react-router";
import type { Schemas } from "../../api/client";
import { useTableMaintenance } from "../../api/queries";
import { ErrorNotice } from "../../components/ErrorNotice";
import { Metric, MetricGrid } from "../../components/Metric";
import { EmptyState, LoadMore, LoadingRows } from "../../components/Page";
import { StatusBadge } from "../../components/StatusBadge";
import { executionStatus, publicationStatus } from "../../jobs/status";
import { CopyButton } from "../../components/ResourceId";
import {
    Card,
    CardContent,
    CardDescription,
    CardHeader,
    CardTitle,
} from "../../ui/Card";
import {
    Table,
    TableBody,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "../../ui/Table";
import {
    date,
    exactBytes,
    exactCount,
    relativeTime,
    shortId,
    timestamp,
} from "../../format";
import { WrenchIcon } from "lucide-react";

/** End-to-end job time, as minutes and seconds. */
function duration(millis: string): string {
    const seconds = Number(millis) / 1000;
    if (seconds < 60) {
        return `${seconds.toFixed(0)} s`;
    }
    const minutes = Math.floor(seconds / 60);
    return `${minutes} min ${Math.round(seconds % 60)} s`;
}

/** A terminal entry's two status lines, as the job page presents them. */
function outcome(entry: Schemas["MaintenanceEntry"]) {
    const boundary = {
        state: entry.state,
        operation: entry.operation,
        publication_started: false,
    };
    return {
        execution: executionStatus(boundary),
        publication: publicationStatus(boundary),
    };
}

/** The table's maintenance tab. */
export function TableMaintenance({ id }: { id: string }) {
    const maintenance = useTableMaintenance(id);
    if (maintenance.isPending) {
        return <LoadingRows label="Loading maintenance history…" />;
    }
    if (maintenance.error !== null) {
        return (
            <ErrorNotice
                error={maintenance.error}
                onRetry={() => {
                    void maintenance.refetch();
                }}
            />
        );
    }
    const items = maintenance.data.pages.flatMap((page) => page.items);
    const first = maintenance.data.pages[0];
    const totals = first?.totals ?? null;
    const since = first?.since ?? null;
    if (items.length === 0) {
        return (
            <EmptyState icon={WrenchIcon} title="No maintenance recorded">
                No terminal maintenance job ran in the last thirty days. Set a
                schedule or run maintenance to start the history.
            </EmptyState>
        );
    }
    return (
        <div className="grid gap-6">
            {totals !== null && (
                <Card>
                    <CardHeader>
                        <CardTitle>Impact</CardTitle>
                        <CardDescription>
                            {since === null
                                ? "Accepted work over the range."
                                : `Accepted work since ${date(since)}, from receipts and verified outputs.`}{" "}
                            A retried task counts once.
                        </CardDescription>
                    </CardHeader>
                    <CardContent>
                        <MetricGrid>
                            <Metric
                                label="Jobs"
                                value={exactCount(totals.jobs)}
                                detail={`${exactCount(totals.succeeded)} published, ${exactCount(totals.replanned)} replanned`}
                            />
                            <Metric
                                label="Files compacted"
                                value={exactCount(totals.input_files)}
                                detail="Accepted inputs"
                            />
                            <Metric
                                label="Bytes rewritten"
                                value={exactBytes(totals.input_bytes)}
                                detail="Accepted inputs"
                            />
                            <Metric
                                label="Output size"
                                value={exactBytes(totals.output_bytes)}
                                detail={`${exactCount(totals.output_files)} published files`}
                            />
                            <Metric
                                label="Delete files applied"
                                value={exactCount(totals.delete_files_applied)}
                            />
                            <Metric
                                label="Work discarded"
                                value={exactBytes(totals.discarded_bytes)}
                                detail={`${exactCount(totals.discarded_files)} files, after conflicts`}
                            />
                            <Metric
                                label="Stopped output"
                                value={exactBytes(totals.abandoned_bytes)}
                                detail={`${exactCount(totals.abandoned_files)} files from stopped jobs`}
                            />
                        </MetricGrid>
                    </CardContent>
                </Card>
            )}

            <Card>
                <CardHeader>
                    <CardTitle>Recent jobs</CardTitle>
                    <CardDescription>
                        Terminal maintenance jobs, newest first. Only work with
                        an accepted receipt is counted.
                    </CardDescription>
                </CardHeader>
                <CardContent>
                    <Table containerLabel="Maintenance history">
                        <TableHeader>
                            <TableRow>
                                <TableHead>Job</TableHead>
                                <TableHead>Execution</TableHead>
                                <TableHead>Publication</TableHead>
                                <TableHead>Created</TableHead>
                                <TableHead className="text-right">
                                    Duration
                                </TableHead>
                                <TableHead className="text-right">
                                    Files compacted
                                </TableHead>
                                <TableHead className="text-right">
                                    Bytes rewritten
                                </TableHead>
                                <TableHead className="text-right">
                                    Output size
                                </TableHead>
                                <TableHead className="text-right">
                                    Delete files
                                </TableHead>
                            </TableRow>
                        </TableHeader>
                        <TableBody>
                            {items.map((entry) => {
                                const status = outcome(entry);
                                return (
                                    <TableRow key={entry.job_id}>
                                        <TableCell>
                                            <span className="flex items-center gap-2">
                                                <Link
                                                    to="/jobs/$jobId"
                                                    params={{
                                                        jobId: entry.job_id,
                                                    }}
                                                    title={entry.job_id}
                                                    className="hover:text-foreground"
                                                >
                                                    Job {shortId(entry.job_id)}
                                                </Link>
                                                <CopyButton
                                                    value={entry.job_id}
                                                    label="job ID"
                                                />
                                            </span>
                                        </TableCell>
                                        <TableCell>
                                            <StatusBadge
                                                status={status.execution}
                                            />
                                        </TableCell>
                                        <TableCell>
                                            <StatusBadge
                                                status={status.publication}
                                            />
                                        </TableCell>
                                        <TableCell className="whitespace-nowrap text-muted-foreground">
                                            {timestamp(entry.created_at)}
                                            <span className="ml-2 text-xs">
                                                {relativeTime(entry.created_at)}
                                            </span>
                                        </TableCell>
                                        <TableCell className="text-right tabular-nums">
                                            {duration(entry.elapsed_millis)}
                                        </TableCell>
                                        <TableCell className="text-right tabular-nums">
                                            {exactCount(entry.input_files)}
                                        </TableCell>
                                        <TableCell className="text-right tabular-nums">
                                            {exactBytes(entry.input_bytes)}
                                        </TableCell>
                                        <TableCell className="text-right tabular-nums">
                                            {exactBytes(entry.output_bytes)}
                                        </TableCell>
                                        <TableCell className="text-right tabular-nums">
                                            {exactCount(
                                                entry.delete_files_applied,
                                            )}
                                        </TableCell>
                                    </TableRow>
                                );
                            })}
                        </TableBody>
                    </Table>
                    {maintenance.hasNextPage && (
                        <LoadMore
                            shown={items.length}
                            noun="job"
                            hasMore={maintenance.hasNextPage}
                            loading={maintenance.isFetchingNextPage}
                            onLoad={() => {
                                void maintenance.fetchNextPage();
                            }}
                        />
                    )}
                </CardContent>
            </Card>
        </div>
    );
}
