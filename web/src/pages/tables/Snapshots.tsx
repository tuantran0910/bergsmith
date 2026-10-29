// The table detail's snapshot timeline (BS-026.T1): every recorded commit,
// its operation, the change counts its summary records, the properties the
// committing engine wrote, and the Bergsmith job that published it. The
// commit-per-hour chart above the list is the table's write cadence, which is
// what a schedule window has to fit around.

import { Link } from "@tanstack/react-router";
import { HistoryIcon } from "lucide-react";

import type { Schemas } from "../../api/client";
import { useTableSnapshots } from "../../api/queries";
import { ErrorNotice } from "../../components/ErrorNotice";
import { DefinitionList } from "../../components/Metric";
import { EmptyState, LoadMore, LoadingRows } from "../../components/Page";
import { TechnicalDetails } from "../../components/TechnicalDetails";
import {
    Card,
    CardContent,
    CardDescription,
    CardHeader,
    CardTitle,
} from "../../ui/Card";
import { Badge } from "../../ui/Badge";
import {
    count,
    exactCount,
    relativeTime,
    shortId,
    timestamp,
} from "../../format";

/** The UTC hour of an instant, as `HH:00`. The chart is labelled in UTC. */
function utcHour(value: string): string {
    const date = new Date(value);
    const hours = String(date.getUTCHours()).padStart(2, "0");
    return `${hours}:00`;
}

/** The tone of an operation badge; the text always names the operation. */
function operationTone(
    operation: string,
): "success" | "info" | "warning" | "destructive" | "neutral" {
    switch (operation) {
        case "append":
            return "info";
        case "replace":
            return "success";
        case "overwrite":
            return "warning";
        case "delete":
            return "destructive";
        default:
            return "neutral";
    }
}

/**
 * What one commit changed, from the counts its summary records. A commit that
 * changes only metadata reports that rather than looking like an empty one.
 */
function change(delta: Schemas["SnapshotDelta"]): string {
    const parts: string[] = [];

    function added(amount: number, noun: string): void {
        if (amount > 0) {
            parts.push(`+${count(amount, noun)}`);
        }
    }
    function removed(amount: number, noun: string): void {
        if (amount > 0) {
            parts.push(`−${count(amount, noun)}`);
        }
    }

    added(Number(delta.added_data_files), "data file");
    removed(Number(delta.deleted_data_files), "data file");
    added(Number(delta.added_delete_files), "delete file");
    removed(Number(delta.removed_delete_files), "delete file");
    const addedRecords = BigInt(delta.added_records);
    if (addedRecords > 0n) {
        parts.push(`+${exactCount(delta.added_records)} rows`);
    }
    const removedRecords = BigInt(delta.deleted_records);
    if (removedRecords > 0n) {
        parts.push(`−${exactCount(delta.deleted_records)} rows`);
    }
    return parts.length === 0
        ? "No data files or rows changed"
        : parts.join(", ");
}

/**
 * The committing engine's summary properties. Bergsmith's own keys are only
 * present for a caller allowed to read the table's jobs; the job link says
 * the same thing in prose.
 */
function properties(snapshot: Schemas["TableSnapshot"]) {
    return Object.entries(snapshot.properties).sort(([left], [right]) =>
        left.localeCompare(right),
    );
}

/** One commit's identifying details, collapsed behind a disclosure. */
function SnapshotDetails({ snapshot }: { snapshot: Schemas["TableSnapshot"] }) {
    const engine = properties(snapshot);
    return (
        <TechnicalDetails>
            <DefinitionList
                items={[
                    { term: "Snapshot", description: snapshot.snapshot_id },
                    {
                        term: "Parent",
                        description: snapshot.parent_snapshot_id ?? "None",
                    },
                    {
                        term: "Sequence",
                        description: snapshot.sequence_number,
                    },
                    ...engine.map(([key, value]) => ({
                        term: key,
                        description: value,
                    })),
                ]}
            />
        </TechnicalDetails>
    );
}

/** The engine or job that made the commit, as one line of text. */
function attribution(snapshot: Schemas["TableSnapshot"]) {
    const engine = snapshot.properties["engine-name"];
    const version = snapshot.properties["engine-version"];
    const engineLabel =
        engine === undefined
            ? undefined
            : version === undefined
              ? engine
              : `${engine} ${version}`;
    if (snapshot.job_id !== null) {
        return (
            <span>
                Published by Bergsmith ·{" "}
                <Link
                    to="/jobs/$jobId"
                    params={{ jobId: snapshot.job_id }}
                    className="underline underline-offset-2 hover:text-foreground"
                >
                    job {shortId(snapshot.job_id)}
                </Link>
            </span>
        );
    }
    return <span>Committed by {engineLabel ?? "another engine"}</span>;
}

/** Commits per hour as vertical bars, in SVG attributes for the CSP. */
function CadenceChart({ cadence }: { cadence: Schemas["SnapshotCadence"] }) {
    const counts = cadence.buckets.map((bucket) => Number(bucket.commits));
    const total = counts.reduce((sum, commits) => sum + commits, 0);
    const peak = counts.length === 0 ? 0 : Math.max(...counts);
    // The denominator keeps a bar visible at one commit; it is not a measured
    // value and never appears in the text.
    const scale = Math.max(1, peak);
    const busiest = peak === 0 ? -1 : counts.indexOf(peak);
    const height = 80;
    const slot = 16;
    const width = Math.max(slot, counts.length * slot);
    const first = cadence.buckets[0];
    const last = cadence.buckets[cadence.buckets.length - 1];
    const range =
        first === undefined || last === undefined
            ? "the requested window"
            : `the hours starting ${utcHour(first.start)} to ${utcHour(last.start)} UTC`;
    const summary =
        total === 0
            ? `No commits in the last ${count(cadence.hours, "hour")}.`
            : `${count(total, "commit")} in the last ${count(cadence.hours, "hour")}, at most ${count(peak, "commit")} in the hour starting ${utcHour(cadence.buckets[busiest]?.start ?? "")} UTC.`;
    return (
        <figure className="m-0">
            <svg
                viewBox={`0 0 ${width} ${height + 18}`}
                role="img"
                aria-label={`Commits per hour, ${range}. ${summary}`}
                className="w-full max-w-3xl"
            >
                {cadence.buckets.map((bucket, index) => {
                    const commits = counts[index] ?? 0;
                    const bar =
                        commits === 0
                            ? 0
                            : Math.max(2, (commits / scale) * height);
                    return (
                        <rect
                            key={bucket.start}
                            x={index * slot + 2}
                            y={height - bar}
                            width={slot - 4}
                            height={bar}
                            rx={2}
                            className={
                                commits === 0 ? "fill-muted" : "fill-primary"
                            }
                        />
                    );
                })}
                <text
                    x={0}
                    y={height + 12}
                    fontSize="10"
                    className="fill-muted-foreground"
                >
                    {utcHour(first?.start ?? "")}
                </text>
                <text
                    x={width}
                    y={height + 12}
                    fontSize="10"
                    textAnchor="end"
                    className="fill-muted-foreground"
                >
                    {utcHour(last?.start ?? "")} UTC
                </text>
            </svg>
            <figcaption className="mt-1 text-xs text-muted-foreground">
                {summary}
            </figcaption>
        </figure>
    );
}

export function TableSnapshots({ id }: { id: string }) {
    const snapshots = useTableSnapshots(id);
    if (snapshots.isPending) {
        return <LoadingRows label="Loading commits…" />;
    }
    if (snapshots.error !== null) {
        return (
            <ErrorNotice
                error={snapshots.error}
                onRetry={() => {
                    void snapshots.refetch();
                }}
            />
        );
    }
    const commits = snapshots.data.pages.flatMap((page) => page.items);
    const cadence = snapshots.data.pages[0]?.cadence;
    if (cadence === undefined) {
        return <LoadingRows label="Loading commits…" />;
    }
    return (
        <div className="grid gap-6">
            <Card>
                <CardHeader>
                    <CardTitle>Commit cadence</CardTitle>
                    <CardDescription>
                        Commits recorded in the last{" "}
                        {count(cadence.hours, "hour")}, ending with the current
                        hour. Schedule windows have to fit around this activity.
                    </CardDescription>
                </CardHeader>
                <CardContent>
                    <CadenceChart cadence={cadence} />
                </CardContent>
            </Card>

            <Card>
                <CardHeader>
                    <CardTitle>Commits</CardTitle>
                    <CardDescription>
                        Every snapshot recorded from the table's metadata,
                        newest first. Bergsmith records them when it samples
                        statistics, so the newest commits can lag by one
                        interval.
                    </CardDescription>
                </CardHeader>
                <CardContent>
                    {commits.length === 0 ? (
                        <EmptyState
                            icon={HistoryIcon}
                            title="No commits recorded yet"
                        >
                            The coordinator records a table's snapshot log when
                            it samples statistics. Commits appear after the
                            first pass over this table.
                        </EmptyState>
                    ) : (
                        <ol aria-label="Commits" className="m-0 list-none p-0">
                            {commits.map((snapshot) => (
                                <li
                                    key={snapshot.snapshot_id}
                                    className="grid gap-1 border-b py-3 last:border-b-0"
                                >
                                    <div className="flex flex-wrap items-center gap-2">
                                        <Badge
                                            variant={operationTone(
                                                snapshot.operation,
                                            )}
                                        >
                                            {snapshot.operation}
                                        </Badge>
                                        <time
                                            dateTime={snapshot.committed_at}
                                            className="text-sm font-semibold"
                                        >
                                            {timestamp(snapshot.committed_at)}
                                        </time>
                                        <span className="text-xs text-muted-foreground">
                                            {relativeTime(
                                                snapshot.committed_at,
                                            )}
                                        </span>
                                    </div>
                                    <p className="m-0 text-sm">
                                        {change(snapshot.delta)}
                                    </p>
                                    <p className="m-0 text-xs text-muted-foreground">
                                        {attribution(snapshot)}
                                    </p>
                                    <SnapshotDetails snapshot={snapshot} />
                                </li>
                            ))}
                        </ol>
                    )}
                    {snapshots.hasNextPage && (
                        <LoadMore
                            shown={commits.length}
                            noun="commit"
                            hasMore={snapshots.hasNextPage}
                            loading={snapshots.isFetchingNextPage}
                            onLoad={() => {
                                void snapshots.fetchNextPage();
                            }}
                        />
                    )}
                </CardContent>
            </Card>
        </div>
    );
}
