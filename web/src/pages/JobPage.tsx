import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import {
    CircleAlertIcon,
    OctagonXIcon,
    RefreshCwIcon,
    RotateCcwIcon,
} from "lucide-react";
import { type ReactNode, useId, useState } from "react";

import { type Job, api, call, csrfHeader } from "../api/client";
import { hasTablePermission } from "../api/auth";
import { useKeyedMutation } from "../api/keyed";
import { useCatalogNames, useJob } from "../api/queries";
import { useBreadcrumbLabel } from "../components/Breadcrumbs";
import { ErrorNotice } from "../components/ErrorNotice";
import { LoadingDetail, PageHeader } from "../components/Page";
import { ResourceId } from "../components/ResourceId";
import { StatusBadge } from "../components/StatusBadge";
import { TechnicalDetails } from "../components/TechnicalDetails";
import { shortId, tableName, timestamp, words } from "../format";
import { type Connection, jobQueryKey, useJobEvents } from "../jobs/events";
import {
    type StatusLine,
    canCancel,
    canRetry,
    cancellationMessage,
    executionStatus,
    isTerminal,
    notice,
    publicationStatus,
} from "../jobs/status";
import { useSession, useSessionMutation } from "../queryClient";
import { Alert, AlertDescription, AlertTitle } from "../ui/Alert";
import { Button } from "../ui/Button";
import {
    Card,
    CardContent,
    CardDescription,
    CardHeader,
    CardTitle,
} from "../ui/Card";
import {
    Dialog,
    DialogClose,
    DialogContent,
    DialogDescription,
    DialogFooter,
    DialogHeader,
    DialogTitle,
    DialogTrigger,
} from "../ui/Dialog";
import { Input, Label } from "../ui/Input";
import { cn } from "../ui/cn";
import { JobStatistics } from "./jobs/JobStatistics";

/** One job: live progress, cancellation, and fresh-plan retry. */
export function JobPage({ id }: { id: string }) {
    const job = useJob(id);

    if (job.isPending) {
        return <LoadingDetail label="Loading job…" />;
    }
    if (job.error) {
        return (
            <ErrorNotice
                error={job.error}
                onRetry={() => {
                    void job.refetch();
                }}
            />
        );
    }
    return <JobDetail job={job.data} />;
}

function JobDetail({ job }: { job: Job }) {
    const queryClient = useQueryClient();
    const navigate = useNavigate();
    const terminal = isTerminal(job.state);
    const live = useJobEvents(job.id, terminal);
    const catalogs = useCatalogNames();
    const names = catalogs.data ?? new Map<string, string>();
    const name = tableName(job.table_aliases, job.table_id, names);
    useBreadcrumbLabel(`Job ${shortId(job.id)}`);
    const attention = notice(job.state);
    const session = useSession();
    const connections = job.table_aliases.map((alias) => alias.catalog_id);
    const cancelAllowed =
        session !== null &&
        hasTablePermission(session, "jobs:cancel", job.table_id, connections);
    const retryAllowed =
        session !== null &&
        hasTablePermission(session, "jobs:retry", job.table_id, connections);

    const retry = useKeyedMutation(
        (key: string) =>
            call(
                api.POST("/api/v1/jobs/{id}/retry", {
                    params: {
                        path: { id: job.id },
                        header: { "Idempotency-Key": key, ...csrfHeader() },
                    },
                }),
            ),
        (accepted) => {
            void queryClient.invalidateQueries({ queryKey: ["jobs"] });
            void navigate({
                to: "/jobs/$jobId",
                params: { jobId: accepted.job_id },
            });
        },
    );

    return (
        <section className="space-y-6">
            <PageHeader
                title={name}
                description={
                    <span className="inline-flex items-center gap-1">
                        Job <ResourceId id={job.id} label="job ID" short />
                    </span>
                }
                actions={
                    <>
                        <CancelJob
                            job={job}
                            allowed={canCancel(job) && cancelAllowed}
                        />
                        {canRetry(job) && retryAllowed && (
                            <Button
                                disabled={retry.isPending}
                                onClick={() => {
                                    retry.mutate(undefined);
                                }}
                            >
                                <RotateCcwIcon aria-hidden="true" />
                                Retry with a fresh plan
                            </Button>
                        )}
                    </>
                }
            />
            {retry.error !== null && (
                <ErrorNotice
                    error={retry.error}
                    onRetry={() => {
                        retry.mutate(undefined);
                    }}
                />
            )}

            {attention && (
                <Alert role="status" variant="warning">
                    <CircleAlertIcon aria-hidden="true" />
                    <AlertTitle>{attention.title}</AlertTitle>
                    <AlertDescription>
                        <p>{attention.action}</p>
                    </AlertDescription>
                </Alert>
            )}

            <div className="grid gap-4 md:grid-cols-2">
                <Lane title="Execution" status={executionStatus(job)}>
                    {job.operation === "expire_snapshots"
                        ? "Metadata-only work: no worker tasks."
                        : "The rewrite work that workers do."}
                </Lane>
                <Lane title="Publication" status={publicationStatus(job)}>
                    The catalog commit. Only a confirmed publication changed the
                    table.
                </Lane>
            </div>

            <JobStatistics statistics={job.statistics} />
            <JobFacts job={job} names={names} />

            <Card>
                <CardHeader className="flex flex-wrap items-center justify-between gap-2">
                    <CardTitle>Events</CardTitle>
                    <ConnectionState
                        connection={live.connection}
                        terminal={terminal}
                        onReconnect={live.reconnect}
                    />
                </CardHeader>
                <CardContent>
                    {live.events.length === 0 && (
                        <p className="m-0 text-muted-foreground">
                            No events received yet.
                        </p>
                    )}
                    <ol
                        aria-label="Job events"
                        className="m-0 grid list-none gap-0 p-0"
                    >
                        {live.events.map((event) => (
                            <li
                                key={event.sequence}
                                className="relative border-l pb-4 pl-5 last:border-transparent last:pb-0"
                            >
                                <span
                                    aria-hidden="true"
                                    className="absolute top-1.5 -left-[5px] size-2.5 rounded-full bg-primary ring-4 ring-card"
                                />
                                <span className="font-medium">
                                    {words(event.kind)}
                                    {event.from_state !== null &&
                                        event.from_state !== event.to_state && (
                                            <>
                                                : {event.from_state} →{" "}
                                                {event.to_state}
                                            </>
                                        )}
                                    {event.kind === "created" && (
                                        <>: {event.to_state}</>
                                    )}
                                    {event.reason !== null && (
                                        <> ({words(event.reason)})</>
                                    )}
                                </span>
                                <span className="block text-xs text-muted-foreground tabular-nums">
                                    {timestamp(event.occurred_at)}
                                </span>
                            </li>
                        ))}
                    </ol>
                </CardContent>
            </Card>
        </section>
    );
}

/** One side of a job's outcome, with what that side means. */
function Lane({
    title,
    status,
    children,
}: {
    title: string;
    status: StatusLine;
    children: ReactNode;
}) {
    return (
        <Card>
            <CardHeader>
                <div className="flex flex-wrap items-center justify-between gap-2">
                    <CardTitle className="m-0">{title}</CardTitle>
                    <StatusBadge status={status} />
                </div>
                <CardDescription className="m-0">{children}</CardDescription>
            </CardHeader>
        </Card>
    );
}

function JobFacts({
    job,
    names,
}: {
    job: Job;
    names: ReadonlyMap<string, string>;
}) {
    const name = tableName(job.table_aliases, job.table_id, names);
    const technical = [
        {
            term: "Job ID",
            description: <ResourceId id={job.id} label="full job ID" />,
        },
        {
            term: "Table ID",
            description: <ResourceId id={job.table_id} label="table ID" />,
        },
        {
            term: "Requested by ID",
            description: (
                <ResourceId id={job.created_by} label="requester ID" />
            ),
        },
        ...(job.retry_of_job_id === null
            ? []
            : [
                  {
                      term: "Retry of job ID",
                      description: (
                          <ResourceId
                              id={job.retry_of_job_id}
                              label="retrying job ID"
                          />
                      ),
                  },
              ]),
    ];

    return (
        <Card>
            <CardContent className="grid gap-4">
                <dl className="m-0 grid gap-x-6 gap-y-3 sm:grid-cols-[max-content_1fr]">
                    <dt className="text-muted-foreground">Operation</dt>
                    <dd className="m-0">
                        {job.operation === "expire_snapshots"
                            ? "Snapshot expiration"
                            : "Compaction"}
                    </dd>
                    <dt className="text-muted-foreground">State</dt>
                    <dd className="m-0">
                        {job.state} (revision {job.revision})
                        {job.state_reason !== null && (
                            <> — {words(job.state_reason)}</>
                        )}
                    </dd>
                    <dt className="text-muted-foreground">Table</dt>
                    <dd className="m-0">
                        <Link
                            to="/tables/$tableId"
                            params={{ tableId: job.table_id }}
                        >
                            {name}
                        </Link>
                    </dd>
                    <dt className="text-muted-foreground">Requested by</dt>
                    <dd className="m-0">{job.created_by_name}</dd>
                    {job.retry_of_job_id !== null && (
                        <>
                            <dt className="text-muted-foreground">Retry of</dt>
                            <dd className="m-0">
                                <Link
                                    to="/jobs/$jobId"
                                    params={{ jobId: job.retry_of_job_id }}
                                    title={job.retry_of_job_id}
                                >
                                    Job {shortId(job.retry_of_job_id)}
                                </Link>
                            </dd>
                        </>
                    )}
                    <dt className="text-muted-foreground">Cancellation</dt>
                    <dd className="m-0">
                        {job.cancellation_requested
                            ? "Requested"
                            : "Not requested"}
                    </dd>
                    <dt className="text-muted-foreground">Created</dt>
                    <dd className="m-0">{timestamp(job.created_at)}</dd>
                    <dt className="text-muted-foreground">Updated</dt>
                    <dd className="m-0">{timestamp(job.updated_at)}</dd>
                </dl>
                <TechnicalDetails items={technical} />
            </CardContent>
        </Card>
    );
}

const CONNECTION_DOT: Record<Connection, string> = {
    open: "bg-success",
    connecting: "bg-muted-foreground",
    reconnecting: "bg-warning",
    closed: "bg-muted-foreground",
};

/** Whether events arrive live, and a way back when they stopped. */
function ConnectionState({
    connection,
    terminal,
    onReconnect,
}: {
    connection: Connection;
    terminal: boolean;
    onReconnect: () => void;
}) {
    return (
        <p className="m-0 flex items-center gap-2 text-sm text-muted-foreground">
            <span
                aria-hidden="true"
                className={cn(
                    "size-2 rounded-full",
                    CONNECTION_DOT[connection],
                    connection === "open" &&
                        "animate-pulse motion-reduce:animate-none",
                )}
            />
            {connection === "open" && "Live."}
            {connection === "connecting" && "Connecting…"}
            {connection === "reconnecting" && "Reconnecting…"}
            {connection === "closed" &&
                (terminal ? "The job has ended." : "Live updates stopped.")}
            {connection === "closed" && !terminal && (
                <Button variant="outline" size="sm" onClick={onReconnect}>
                    <RefreshCwIcon aria-hidden="true" />
                    Reconnect
                </Button>
            )}
        </p>
    );
}

/**
 * Asks to cancel through a confirmation dialog, which then shows what the
 * request achieved. The dialog stays until closed, even when the job ends
 * meanwhile, so the reported outcome is not lost.
 */
function CancelJob({ job, allowed }: { job: Job; allowed: boolean }) {
    const queryClient = useQueryClient();
    const [open, setOpen] = useState(false);
    const [reason, setReason] = useState("");
    const reasonId = useId();

    const cancel = useSessionMutation({
        mutationFn: () =>
            call(
                api.POST("/api/v1/jobs/{id}/cancel", {
                    params: { path: { id: job.id }, header: csrfHeader() },
                    body: {
                        reason: reason.trim() === "" ? null : reason.trim(),
                    },
                }),
            ),
        onSuccess: () => {
            void queryClient.invalidateQueries({
                queryKey: jobQueryKey(job.id),
            });
        },
    });

    if (!allowed && !open) {
        return null;
    }
    const outcome = cancel.data
        ? cancellationMessage(cancel.data.cancellation)
        : null;

    return (
        <Dialog
            open={open}
            onOpenChange={(next) => {
                if (next) {
                    setReason("");
                    cancel.reset();
                }
                setOpen(next);
            }}
        >
            {allowed && (
                <DialogTrigger render={<Button variant="destructive" />}>
                    <OctagonXIcon aria-hidden="true" />
                    Cancel job
                </DialogTrigger>
            )}
            <DialogContent>
                <form
                    className="grid gap-4"
                    onSubmit={(event) => {
                        event.preventDefault();
                        cancel.mutate();
                    }}
                >
                    <DialogHeader>
                        <DialogTitle>Cancel this job?</DialogTitle>
                        <DialogDescription>
                            Cancellation is a request. Once a commit is
                            authorized it can be too late, and the job may still
                            publish.
                        </DialogDescription>
                    </DialogHeader>
                    {outcome === null ? (
                        <div className="grid gap-1.5">
                            <Label htmlFor={reasonId}>
                                Cancellation reason (optional)
                            </Label>
                            <Input
                                id={reasonId}
                                value={reason}
                                maxLength={500}
                                onChange={(event) => {
                                    setReason(event.target.value);
                                }}
                            />
                        </div>
                    ) : (
                        <Alert
                            role="status"
                            variant={
                                outcome.tone === "ok"
                                    ? "success"
                                    : outcome.tone === "warning"
                                      ? "warning"
                                      : "neutral"
                            }
                        >
                            <AlertTitle>{outcome.label}</AlertTitle>
                        </Alert>
                    )}
                    {cancel.error !== null && (
                        <ErrorNotice
                            error={cancel.error}
                            onRetry={() => {
                                cancel.mutate();
                            }}
                        />
                    )}
                    <DialogFooter>
                        <DialogClose render={<Button variant="outline" />}>
                            {outcome === null ? "Keep running" : "Done"}
                        </DialogClose>
                        {outcome === null && (
                            <Button
                                type="submit"
                                variant="destructive"
                                disabled={cancel.isPending}
                            >
                                Request cancellation
                            </Button>
                        )}
                    </DialogFooter>
                </form>
            </DialogContent>
        </Dialog>
    );
}
