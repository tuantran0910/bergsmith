// A job's tasks and attempts for triage (BS-032.T5): which task failed, on
// which attempt, with what reason, how long it ran, and what it produced.

import { ListChecksIcon } from "lucide-react";

import type { Job, Schemas } from "../../api/client";
import { useJobTasks } from "../../api/queries";
import { DataTable, type DataTableColumn } from "../../components/DataTable";
import { ErrorNotice } from "../../components/ErrorNotice";
import { EmptyState, LoadMore, LoadingRows } from "../../components/Page";
import { ResourceId } from "../../components/ResourceId";
import { StatusBadge } from "../../components/StatusBadge";
import {
    duration,
    exactBytes,
    exactCount,
    timestamp,
    words,
} from "../../format";
import type { Tone } from "../../jobs/status";

type JobTask = Schemas["JobTask"];
type JobAttempt = Schemas["JobAttempt"];

function toneFor(state: string): Tone {
    switch (state) {
        case "Accepted":
            return "ok";
        case "Failed":
            return "error";
        case "Leased":
        case "Granted":
        case "Running":
            return "progress";
        case "RetryWaiting":
        case "Expired":
            return "warning";
        default:
            return "neutral";
    }
}

function attemptDuration(attempt: JobAttempt): number {
    return attempt.duration_millis === null
        ? 0
        : Number(attempt.duration_millis);
}

function taskSeconds(task: JobTask): number {
    return task.attempts.reduce(
        (total, attempt) => total + attemptDuration(attempt),
        0,
    );
}

function Attempts({ task }: { task: JobTask }) {
    if (task.attempts.length === 0) {
        return <span className="text-muted-foreground">No attempts yet.</span>;
    }
    return (
        <ul className="m-0 grid list-none gap-1.5 p-0">
            {task.attempts.map((attempt) => (
                <li key={attempt.id} className="text-sm">
                    <span className="font-medium">
                        Attempt {attempt.generation}
                    </span>{" "}
                    <StatusBadge
                        status={{
                            label: attempt.state,
                            tone: toneFor(attempt.state),
                        }}
                    />
                    <span className="block text-xs text-muted-foreground">
                        {attempt.pool}/{attempt.worker_name}
                        {attempt.duration_millis !== null && (
                            <> · {duration(attemptDuration(attempt))}</>
                        )}
                        {attempt.finished_at !== null && (
                            <> · finished {timestamp(attempt.finished_at)}</>
                        )}
                        {attempt.output_files !== "0" && (
                            <>
                                {" "}
                                · {exactCount(attempt.output_files)} files ·{" "}
                                {exactBytes(attempt.output_bytes)}
                            </>
                        )}
                    </span>
                    {attempt.failure_reason !== null && (
                        <span className="block text-destructive">
                            {words(attempt.failure_reason)}
                            {attempt.failure_disposition !== null && (
                                <> · {words(attempt.failure_disposition)}</>
                            )}
                        </span>
                    )}
                </li>
            ))}
        </ul>
    );
}

/** One job's tasks, attempts, failure reasons, durations, and outputs. */
export function JobTasks({ job, active }: { job: Job; active: boolean }) {
    const tasks = useJobTasks(job.id, active);
    if (tasks.isPending && active) {
        return <LoadingRows label="Loading tasks…" />;
    }
    if (tasks.error) {
        return (
            <ErrorNotice
                error={tasks.error}
                onRetry={() => {
                    void tasks.refetch();
                }}
            />
        );
    }
    const items = tasks.data?.pages.flatMap((page) => page.items) ?? [];
    if (items.length === 0) {
        return (
            <EmptyState icon={ListChecksIcon} title="No worker tasks.">
                {job.operation === "remove_orphan_files"
                    ? "Orphan-file cleanup runs in the coordinator and deletes storage objects without worker tasks."
                    : job.operation === "expire_snapshots" ||
                        job.operation === "rewrite_manifests" ||
                        job.operation === "remove_dangling_deletes"
                      ? "This operation publishes metadata only; there are no worker tasks to triage."
                      : "The plan has not produced tasks yet. Progress and events stay on the Overview tab."}
            </EmptyState>
        );
    }

    const columns: DataTableColumn<JobTask>[] = [
        {
            id: "task",
            header: "Task",
            sortValue: (task) => task.ordinal,
            cell: (task) => (
                <span className="inline-flex items-center gap-1">
                    <span className="font-medium">Task {task.ordinal + 1}</span>
                    <ResourceId id={task.id} label="task ID" short />
                </span>
            ),
        },
        {
            id: "state",
            header: "State",
            sortValue: (task) => task.state,
            cell: (task) => (
                <StatusBadge
                    status={{ label: task.state, tone: toneFor(task.state) }}
                />
            ),
        },
        {
            id: "attempts",
            header: "Attempts",
            sortValue: (task) => task.attempts_started,
            cell: (task) => <Attempts task={task} />,
        },
        {
            id: "duration",
            header: "Duration",
            align: "end",
            sortValue: (task) => taskSeconds(task),
            cell: (task) =>
                task.attempts.length === 0 ? (
                    <span className="text-muted-foreground">—</span>
                ) : (
                    <span className="tabular-nums">
                        {duration(taskSeconds(task))}
                    </span>
                ),
        },
        {
            id: "outputs",
            header: "Outputs",
            align: "end",
            sortValue: (task) => Number(task.output_bytes),
            cell: (task) => (
                <span className="tabular-nums">
                    {exactCount(task.output_files)} files ·{" "}
                    {exactBytes(task.output_bytes)}
                </span>
            ),
        },
        {
            id: "updated",
            header: "Updated",
            sortValue: (task) => task.updated_at,
            cell: (task) => (
                <time
                    dateTime={task.updated_at}
                    className="whitespace-nowrap text-muted-foreground"
                >
                    {timestamp(task.updated_at)}
                </time>
            ),
        },
    ];

    return (
        <div>
            <DataTable
                rows={items}
                columns={columns}
                getRowKey={(task) => task.id}
                sort={{ column: "task", direction: "asc" }}
                caption="Job tasks"
            />
            <LoadMore
                shown={items.length}
                noun="tasks"
                hasMore={tasks.hasNextPage}
                loading={tasks.isFetchingNextPage}
                onLoad={() => {
                    void tasks.fetchNextPage();
                }}
            />
            {tasks.isFetchingNextPage && (
                <span role="status" className="sr-only">
                    Loading more tasks…
                </span>
            )}
        </div>
    );
}
