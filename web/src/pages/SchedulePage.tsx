// One schedule: its configuration, the next instants the coordinator computed,
// and the durable evaluation history, including why an evaluation was skipped.

import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { CalendarClockIcon } from "lucide-react";

import { canManageSchedules } from "../api/auth";
import type { Schemas } from "../api/client";
import { useSchedule, useScheduleOccurrences } from "../api/schedules";
import { DataTable, type DataTableColumn } from "../components/DataTable";
import { ErrorNotice } from "../components/ErrorNotice";
import { GrantRequired } from "../components/GrantRequired";
import {
    EmptyState,
    LoadMore,
    LoadingDetail,
    LoadingRows,
    PageHeader,
} from "../components/Page";
import { number, shortId, timestamp, zonedTimestamp } from "../format";
import { useSession } from "../queryClient";
import { Badge } from "../ui/Badge";
import { Button } from "../ui/Button";
import { Card, CardContent, CardHeader, CardTitle } from "../ui/Card";
import { clock } from "./schedules/draft";
import { EditScheduleDialog } from "./schedules/EditScheduleDialog";
import { useScheduleLabels } from "./schedules/labels";

/** A stored reason such as `OutsideWindow` as prose. */
function reasonText(reason: string): string {
    const spaced = reason.replace(/([a-z0-9])([A-Z])/g, "$1 $2");
    return spaced.charAt(0).toUpperCase() + spaced.slice(1).toLowerCase();
}

/** One evaluation's state, in the vocabulary the plan uses. */
function stateVariant(
    state: Schemas["ScheduleOccurrence"]["state"],
): "success" | "neutral" | "warning" {
    switch (state) {
        case "Admitted":
            return "success";
        case "Coalesced":
            return "neutral";
        case "Skipped":
            return "warning";
        default:
            return "neutral";
    }
}

/** A schedule's configuration, next instants, and evaluation history. */
export function SchedulePage({ id }: { id: string }) {
    const session = useSession();
    const canManage = session !== null && canManageSchedules(session);
    const schedule = useSchedule(id);
    const occurrences = useScheduleOccurrences(id, canManage);
    const labels = useScheduleLabels(canManage);
    const [editing, setEditing] = useState(false);
    const [notice, setNotice] = useState<string | null>(null);
    const history = occurrences.data?.pages.flatMap((page) => page.items) ?? [];

    if (session !== null && !canManage) {
        return (
            <GrantRequired>
                Schedules are organization configuration. Reading one requires a
                grant over the whole organization.
            </GrantRequired>
        );
    }

    const columns: DataTableColumn<Schemas["ScheduleOccurrence"]>[] = [
        {
            id: "scheduled",
            header: "Evaluation",
            cell: (occurrence) => (
                <span className="grid gap-0.5">
                    <time dateTime={occurrence.scheduled_for}>
                        {timestamp(occurrence.scheduled_for)}
                    </time>
                    {occurrence.coalesced_until !== null && (
                        <span className="text-xs text-muted-foreground">
                            covers through{" "}
                            {timestamp(occurrence.coalesced_until)}
                        </span>
                    )}
                </span>
            ),
        },
        {
            id: "state",
            header: "State",
            cell: (occurrence) => (
                <Badge variant={stateVariant(occurrence.state)}>
                    {occurrence.state}
                </Badge>
            ),
        },
        {
            id: "reason",
            header: "Reason",
            cell: (occurrence) => (
                <span className="text-muted-foreground">
                    {occurrence.reason === null
                        ? "—"
                        : reasonText(occurrence.reason)}
                </span>
            ),
        },
        {
            id: "job",
            header: "Job",
            cell: (occurrence) =>
                occurrence.job_id === null ? (
                    <span className="text-muted-foreground">—</span>
                ) : (
                    <Link
                        to="/jobs/$jobId"
                        params={{ jobId: occurrence.job_id }}
                        className="font-medium"
                    >
                        {shortId(occurrence.job_id)}
                    </Link>
                ),
        },
        {
            id: "recorded",
            header: "Recorded",
            className: "whitespace-nowrap",
            cell: (occurrence) => (
                <span className="text-muted-foreground">
                    {timestamp(occurrence.created_at)}
                </span>
            ),
        },
    ];

    return (
        <section className="grid gap-4">
            <p className="m-0 text-sm">
                <Link to="/schedules">All schedules</Link>
            </p>
            {schedule.isPending && <LoadingDetail label="Loading schedule…" />}
            {schedule.error && (
                <ErrorNotice
                    error={schedule.error}
                    onRetry={() => {
                        void schedule.refetch();
                    }}
                />
            )}
            {schedule.data && (
                <>
                    <PageHeader
                        title={labels.tableName(schedule.data.table_id)}
                        description={
                            <>
                                {labels.policyName(schedule.data.policy_id)} ·{" "}
                                <code>{schedule.data.cron_expression}</code>{" "}
                                {schedule.data.time_zone}
                            </>
                        }
                        actions={
                            canManage && (
                                <Button
                                    size="lg"
                                    onClick={() => {
                                        setNotice(null);
                                        setEditing(true);
                                    }}
                                >
                                    Edit schedule
                                </Button>
                            )
                        }
                    />
                    {notice !== null && (
                        <p role="status" className="m-0 text-sm">
                            {notice}
                        </p>
                    )}
                    <Card>
                        <CardHeader>
                            <CardTitle>Configuration</CardTitle>
                        </CardHeader>
                        <CardContent>
                            <dl className="m-0 grid gap-x-6 gap-y-2 text-sm sm:grid-cols-2">
                                <div>
                                    <dt className="text-muted-foreground">
                                        Policy
                                    </dt>
                                    <dd className="m-0">
                                        {labels.policyName(
                                            schedule.data.policy_id,
                                        )}
                                    </dd>
                                </div>
                                <div>
                                    <dt className="text-muted-foreground">
                                        Service identity
                                    </dt>
                                    <dd className="m-0">
                                        {schedule.data.service_identity_name}
                                    </dd>
                                </div>
                                <div>
                                    <dt className="text-muted-foreground">
                                        Cron
                                    </dt>
                                    <dd className="m-0">
                                        <code>
                                            {schedule.data.cron_expression}
                                        </code>{" "}
                                        in {schedule.data.time_zone}
                                    </dd>
                                </div>
                                <div>
                                    <dt className="text-muted-foreground">
                                        Window
                                    </dt>
                                    <dd className="m-0">
                                        {schedule.data.window === null
                                            ? "Any time"
                                            : `${clock(schedule.data.window.start_minute)}–${clock(schedule.data.window.end_minute)} local time`}
                                    </dd>
                                </div>
                                <div>
                                    <dt className="text-muted-foreground">
                                        State
                                    </dt>
                                    <dd className="m-0">
                                        <Badge>
                                            {schedule.data.enabled
                                                ? "Enabled"
                                                : "Disabled"}
                                        </Badge>
                                    </dd>
                                </div>
                                <div>
                                    <dt className="text-muted-foreground">
                                        Revision
                                    </dt>
                                    <dd className="m-0 tabular-nums">
                                        {number(schedule.data.revision)}
                                    </dd>
                                </div>
                                <div>
                                    <dt className="text-muted-foreground">
                                        Created
                                    </dt>
                                    <dd className="m-0">
                                        {timestamp(schedule.data.created_at)}
                                    </dd>
                                </div>
                                <div>
                                    <dt className="text-muted-foreground">
                                        Updated
                                    </dt>
                                    <dd className="m-0">
                                        {timestamp(schedule.data.updated_at)}
                                    </dd>
                                </div>
                            </dl>
                        </CardContent>
                    </Card>
                    <Card>
                        <CardHeader>
                            <CardTitle>Next instants</CardTitle>
                        </CardHeader>
                        <CardContent className="grid gap-2">
                            {schedule.data.next_occurrences.length === 0 ? (
                                <p className="m-0 text-sm text-muted-foreground">
                                    The expression names no reachable instant,
                                    so this schedule stays dormant.
                                </p>
                            ) : (
                                <ol
                                    aria-label="Next instants"
                                    className="m-0 grid list-none gap-1 p-0 text-sm"
                                >
                                    {schedule.data.next_occurrences.map(
                                        (instant) => (
                                            <li key={instant}>
                                                <time
                                                    dateTime={instant}
                                                    title={instant}
                                                >
                                                    {zonedTimestamp(
                                                        instant,
                                                        schedule.data.time_zone,
                                                    )}
                                                </time>
                                            </li>
                                        ),
                                    )}
                                </ol>
                            )}
                            <p className="m-0 text-xs text-muted-foreground">
                                Computed by the coordinator in{" "}
                                {schedule.data.time_zone}. The daily window can
                                still skip one; the history records it.
                                {!schedule.data.enabled &&
                                    " This schedule is paused, so none of these will be admitted until it is enabled."}
                            </p>
                        </CardContent>
                    </Card>
                    <Card>
                        <CardHeader>
                            <CardTitle>History</CardTitle>
                        </CardHeader>
                        <CardContent className="grid gap-3">
                            {occurrences.isPending && (
                                <LoadingRows label="Loading evaluations…" />
                            )}
                            {occurrences.error && (
                                <ErrorNotice
                                    error={occurrences.error}
                                    onRetry={() => {
                                        void occurrences.refetch();
                                    }}
                                />
                            )}
                            {occurrences.isSuccess && history.length === 0 && (
                                <EmptyState
                                    icon={CalendarClockIcon}
                                    title="No evaluations yet."
                                >
                                    The coordinator records one row each time it
                                    evaluates the schedule, including a skipped
                                    one.
                                </EmptyState>
                            )}
                            {history.length > 0 && (
                                <DataTable
                                    rows={history}
                                    columns={columns}
                                    getRowKey={(occurrence) => occurrence.id}
                                    caption="Schedule evaluations"
                                />
                            )}
                            {occurrences.isSuccess && (
                                <LoadMore
                                    shown={history.length}
                                    noun="evaluations"
                                    hasMore={occurrences.hasNextPage}
                                    loading={occurrences.isFetchingNextPage}
                                    onLoad={() => {
                                        void occurrences.fetchNextPage();
                                    }}
                                />
                            )}
                        </CardContent>
                    </Card>
                    {editing && (
                        <EditScheduleDialog
                            key={schedule.data.id}
                            schedule={schedule.data}
                            policy={labels.policyName(schedule.data.policy_id)}
                            table={labels.tableName(schedule.data.table_id)}
                            onClose={() => {
                                setEditing(false);
                            }}
                            onSaved={(table) => {
                                setNotice(`Saved the schedule for ${table}.`);
                            }}
                        />
                    )}
                </>
            )}
        </section>
    );
}
