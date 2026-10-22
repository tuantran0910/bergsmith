import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";

import { type Job, api, call, csrfHeader } from "../api/client";
import { useKeyedMutation } from "../api/keyed";
import { useJob } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import { StatusBadge } from "../components/StatusBadge";
import { timestamp, words } from "../format";
import { jobQueryKey, useJobEvents } from "../jobs/events";
import {
    canCancel,
    canRetry,
    cancellationMessage,
    executionStatus,
    isTerminal,
    notice,
    publicationStatus,
} from "../jobs/status";
import { useSessionMutation } from "../queryClient";

/** One job: live progress, cancellation, and fresh-plan retry. */
export function JobPage({ id }: { id: string }) {
    const job = useJob(id);

    if (job.isPending) {
        return <p>Loading job…</p>;
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
    const terminal = isTerminal(job.state);
    const live = useJobEvents(job.id, terminal);
    const attention = notice(job.state);

    return (
        <section>
            <h1>
                Job <code>{job.id}</code>
            </h1>
            <dl className="facts">
                <dt>Execution</dt>
                <dd>
                    <StatusBadge status={executionStatus(job)} />
                </dd>
                <dt>Publication</dt>
                <dd>
                    <StatusBadge status={publicationStatus(job)} />
                </dd>
                <dt>State</dt>
                <dd>
                    {job.state} (revision {job.revision})
                    {job.state_reason !== null && (
                        <> — {words(job.state_reason)}</>
                    )}
                </dd>
                <dt>Table</dt>
                <dd>
                    <Link
                        to="/tables/$tableId"
                        params={{ tableId: job.table_id }}
                    >
                        {job.table_id}
                    </Link>
                </dd>
                {job.retry_of_job_id !== null && (
                    <>
                        <dt>Retry of</dt>
                        <dd>
                            <Link
                                to="/jobs/$jobId"
                                params={{ jobId: job.retry_of_job_id }}
                            >
                                {job.retry_of_job_id}
                            </Link>
                        </dd>
                    </>
                )}
                <dt>Cancellation</dt>
                <dd>
                    {job.cancellation_requested ? "Requested" : "Not requested"}
                </dd>
                <dt>Created</dt>
                <dd>{timestamp(job.created_at)}</dd>
                <dt>Updated</dt>
                <dd>{timestamp(job.updated_at)}</dd>
            </dl>

            {attention && (
                <div role="status" className="notice notice-warning">
                    <strong>{attention.title}</strong>
                    <p>{attention.action}</p>
                </div>
            )}

            <JobCommands job={job} />

            <h2>Events</h2>
            <p className="hint">
                {live.connection === "open" && "Live."}
                {live.connection === "connecting" && "Connecting…"}
                {live.connection === "reconnecting" && "Reconnecting…"}
                {live.connection === "closed" &&
                    (terminal ? "The job has ended." : "Live updates stopped.")}
                {live.connection === "closed" && !terminal && (
                    <>
                        {" "}
                        <button type="button" onClick={live.reconnect}>
                            Reconnect
                        </button>
                    </>
                )}
            </p>
            <ol className="events" aria-label="Job events">
                {live.events.map((event) => (
                    <li key={event.sequence}>
                        <span className="event-time">
                            {timestamp(event.occurred_at)}
                        </span>{" "}
                        {words(event.kind)}
                        {event.from_state !== null &&
                            event.from_state !== event.to_state && (
                                <>
                                    : {event.from_state} → {event.to_state}
                                </>
                            )}
                        {event.kind === "created" && <>: {event.to_state}</>}
                        {event.reason !== null && <> ({words(event.reason)})</>}
                    </li>
                ))}
            </ol>
        </section>
    );
}

function JobCommands({ job }: { job: Job }) {
    const queryClient = useQueryClient();
    const navigate = useNavigate();
    const [reason, setReason] = useState("");

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
        <div className="commands">
            {canCancel(job) && (
                <form
                    onSubmit={(event) => {
                        event.preventDefault();
                        cancel.mutate();
                    }}
                >
                    <label>
                        Cancellation reason (optional)
                        <input
                            value={reason}
                            maxLength={500}
                            onChange={(event) => {
                                setReason(event.target.value);
                            }}
                        />
                    </label>
                    <button type="submit" disabled={cancel.isPending}>
                        Request cancellation
                    </button>
                </form>
            )}
            {cancel.data && (
                <p
                    role="status"
                    className={`notice notice-${cancellationMessage(cancel.data.cancellation).tone}`}
                >
                    {cancellationMessage(cancel.data.cancellation).label}
                </p>
            )}
            {cancel.error !== null && (
                <ErrorNotice
                    error={cancel.error}
                    onRetry={() => {
                        cancel.mutate();
                    }}
                />
            )}

            {canRetry(job) && (
                <button
                    type="button"
                    disabled={retry.isPending}
                    onClick={() => {
                        retry.mutate(undefined);
                    }}
                >
                    Retry with a fresh plan
                </button>
            )}
            {retry.error !== null && (
                <ErrorNotice
                    error={retry.error}
                    onRetry={() => {
                        retry.mutate(undefined);
                    }}
                />
            )}
        </div>
    );
}
