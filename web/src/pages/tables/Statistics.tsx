// The table detail's statistics panel (BS-025.T3): the newest metadata-only
// sample, its size histogram, and the history it was sampled from.

import type { Schemas } from "../../api/client";
import { useTableStatistics } from "../../api/queries";
import { ErrorNotice } from "../../components/ErrorNotice";
import { Metric, MetricGrid } from "../../components/Metric";
import { EmptyState, LoadMore, LoadingRows } from "../../components/Page";
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
    timestamp,
} from "../../format";
import { DatabaseIcon } from "lucide-react";

/** A byte range as text, including the open-ended final bucket. */
function bucketLabel(bucket: Schemas["TableStatisticsSizeBucket"]): string {
    const lower = exactBytes(bucket.lower_bound_bytes);
    return bucket.upper_bound_bytes === null
        ? `${lower} and larger`
        : `${lower} to ${exactBytes(bucket.upper_bound_bytes)}`;
}

/**
 * Data-file sizes as horizontal bars. Widths are SVG attributes rather than
 * inline styles, so the coordinator's CSP stays satisfied.
 */
function SizeHistogram({
    buckets,
}: {
    buckets: Schemas["TableStatisticsSizeBucket"][];
}) {
    const files = buckets.map((bucket) => Number(bucket.files));
    const largest = Math.max(1, ...files);
    const row = 22;
    const label = 132;
    const width = 320;
    const bars = buckets.map((bucket, index) => ({
        bucket,
        files: files[index] ?? 0,
        y: index * row,
        bar: ((files[index] ?? 0) / largest) * (width - label - 56),
    }));
    return (
        <svg
            viewBox={`0 0 ${width} ${bars.length * row}`}
            role="img"
            aria-label={`Data file sizes: ${bars
                .map(
                    ({ bucket, files: count }) =>
                        `${bucketLabel(bucket)}: ${count}`,
                )
                .join("; ")}`}
            className="w-full max-w-xl"
        >
            {bars.map(({ bucket, files: count, y, bar }) => (
                <g
                    key={`${bucket.lower_bound_bytes}-${String(bucket.upper_bound_bytes)}`}
                >
                    <text
                        x={0}
                        y={y + 12}
                        fontSize="10"
                        className="fill-muted-foreground"
                    >
                        {bucketLabel(bucket)}
                    </text>
                    <rect
                        x={label}
                        y={y + 2}
                        width={Math.max(count === 0 ? 0 : 2, bar)}
                        height={12}
                        rx={2}
                        className="fill-primary"
                    />
                    <text
                        x={label + Math.max(count === 0 ? 0 : 2, bar) + 6}
                        y={y + 12}
                        fontSize="10"
                        className="fill-foreground"
                    >
                        {exactCount(bucket.files)}
                    </text>
                </g>
            ))}
        </svg>
    );
}

/** The difference between the newest sample and the one before it. */
function change(sample: string, previous: string | undefined): string {
    if (previous === undefined) {
        return "No earlier sample";
    }
    const delta = BigInt(sample) - BigInt(previous);
    if (delta === 0n) {
        return "Unchanged since the previous sample";
    }
    const magnitude = exactBytes((delta < 0n ? -delta : delta).toString());
    return `${delta > 0n ? "+" : "−"}${magnitude} since the previous sample`;
}

export function TableStatistics({ id }: { id: string }) {
    const statistics = useTableStatistics(id);
    if (statistics.isPending) {
        return <LoadingRows label="Loading statistics…" />;
    }
    if (statistics.error !== null) {
        return (
            <ErrorNotice
                error={statistics.error}
                onRetry={() => {
                    void statistics.refetch();
                }}
            />
        );
    }
    const samples = statistics.data.pages.flatMap((page) => page.items);
    const latest = statistics.data.pages[0]?.latest ?? null;
    if (latest === null) {
        return (
            <EmptyState icon={DatabaseIcon} title="No sample yet">
                The coordinator samples every registered table on its interval
                and after each publication. Statistics appear after the first
                pass.
            </EmptyState>
        );
    }
    const previous = samples[1];
    return (
        <div className="grid gap-6">
            <MetricGrid>
                <Metric
                    label="Rows"
                    value={exactCount(latest.record_count)}
                    detail={
                        latest.last_operation === null
                            ? "No commit recorded"
                            : `Last commit: ${latest.last_operation}`
                    }
                />
                <Metric
                    label="Data size"
                    value={exactBytes(latest.data_bytes)}
                    detail={change(latest.data_bytes, previous?.data_bytes)}
                />
                <Metric
                    label="Data files"
                    value={exactCount(latest.data_files)}
                    detail={`${exactCount(latest.small_files)} smaller than ${exactBytes(latest.target_file_size_bytes)}`}
                />
                <Metric
                    label="Delete files"
                    value={exactCount(latest.delete_files)}
                    detail={`${exactCount(latest.position_delete_files)} position, ${exactCount(latest.equality_delete_files)} equality`}
                />
                <Metric
                    label="Partitions"
                    value={exactCount(latest.partition_count)}
                    detail={`${exactCount(latest.manifest_count)} manifests`}
                />
                <Metric
                    label="Snapshots"
                    value={exactCount(latest.snapshot_count)}
                    detail={
                        latest.newest_snapshot_at === null
                            ? "No snapshot log"
                            : `Last commit ${relativeTime(latest.newest_snapshot_at)}`
                    }
                />
            </MetricGrid>

            <Card>
                <CardHeader>
                    <CardTitle>Data-file sizes</CardTitle>
                    <CardDescription>
                        Live data files by size, from the current snapshot's
                        manifests. Sampling never reads a data file.
                    </CardDescription>
                </CardHeader>
                <CardContent>
                    <SizeHistogram buckets={latest.size_histogram} />
                </CardContent>
            </Card>

            <Card>
                <CardHeader>
                    <CardTitle>History</CardTitle>
                    <CardDescription>
                        A sample is recorded when the table's metadata changes.
                        The newest sample was taken {date(latest.sampled_at)}.
                    </CardDescription>
                </CardHeader>
                <CardContent>
                    <Table containerLabel="Statistics history">
                        <TableHeader>
                            <TableRow>
                                <TableHead>Sampled</TableHead>
                                <TableHead className="text-right">
                                    Rows
                                </TableHead>
                                <TableHead className="text-right">
                                    Data files
                                </TableHead>
                                <TableHead className="text-right">
                                    Data size
                                </TableHead>
                                <TableHead className="text-right">
                                    Small files
                                </TableHead>
                                <TableHead className="text-right">
                                    Snapshots
                                </TableHead>
                            </TableRow>
                        </TableHeader>
                        <TableBody>
                            {samples.map((sample) => (
                                <TableRow key={sample.sampled_at}>
                                    <TableCell>
                                        <time dateTime={sample.sampled_at}>
                                            {timestamp(sample.sampled_at)}
                                        </time>
                                    </TableCell>
                                    <TableCell className="text-right tabular-nums">
                                        {exactCount(sample.record_count)}
                                    </TableCell>
                                    <TableCell className="text-right tabular-nums">
                                        {exactCount(sample.data_files)}
                                    </TableCell>
                                    <TableCell className="text-right tabular-nums">
                                        {exactBytes(sample.data_bytes)}
                                    </TableCell>
                                    <TableCell className="text-right tabular-nums">
                                        {exactCount(sample.small_files)}
                                    </TableCell>
                                    <TableCell className="text-right tabular-nums">
                                        {exactCount(sample.snapshot_count)}
                                    </TableCell>
                                </TableRow>
                            ))}
                        </TableBody>
                    </Table>
                    {statistics.hasNextPage && (
                        <LoadMore
                            shown={samples.length}
                            noun="sample"
                            hasMore={statistics.hasNextPage}
                            loading={statistics.isFetchingNextPage}
                            onLoad={() => {
                                void statistics.fetchNextPage();
                            }}
                        />
                    )}
                </CardContent>
            </Card>

            {latest.top_partitions.length > 0 && (
                <Card>
                    <CardHeader>
                        <CardTitle>Busiest partitions</CardTitle>
                        <CardDescription>
                            The partitions with the most data files, for skew,
                            with the delete files that apply to them.
                        </CardDescription>
                    </CardHeader>
                    <CardContent>
                        <Table containerLabel="Busiest partitions">
                            <TableHeader>
                                <TableRow>
                                    <TableHead>Partition</TableHead>
                                    <TableHead className="text-right">
                                        Data files
                                    </TableHead>
                                    <TableHead className="text-right">
                                        Delete files
                                    </TableHead>
                                    <TableHead className="text-right">
                                        Data size
                                    </TableHead>
                                </TableRow>
                            </TableHeader>
                            <TableBody>
                                {latest.top_partitions.map((partition) => (
                                    <TableRow
                                        key={`${partition.spec_id}-${partition.partition}`}
                                    >
                                        <TableCell>
                                            {partition.partition === ""
                                                ? "(unpartitioned)"
                                                : partition.partition}
                                            <span className="ml-2 text-xs text-muted-foreground">
                                                spec {partition.spec_id}
                                            </span>
                                        </TableCell>
                                        <TableCell className="text-right tabular-nums">
                                            {exactCount(partition.files)}
                                        </TableCell>
                                        <TableCell className="text-right tabular-nums">
                                            {exactCount(partition.delete_files)}
                                        </TableCell>
                                        <TableCell className="text-right tabular-nums">
                                            {exactBytes(partition.bytes)}
                                        </TableCell>
                                    </TableRow>
                                ))}
                            </TableBody>
                        </Table>
                    </CardContent>
                </Card>
            )}
        </div>
    );
}
