// How a job's state reads to an operator.
//
// Execution and publication are reported separately: outputs being ready is
// not success. Only `Succeeded` means the catalog publication is confirmed.

import type { Job, JobState, Schemas } from "../api/client";

export type Tone = "progress" | "ok" | "warning" | "error" | "neutral";

export interface StatusLine {
    label: string;
    tone: Tone;
}

export interface Notice {
    title: string;
    action: string;
}

const TERMINAL: ReadonlySet<JobState> = new Set([
    "Succeeded",
    "NoOp",
    "Cancelled",
    "ReplanRequired",
    "Failed",
]);

/** Terminal jobs never change again. */
export function isTerminal(state: JobState): boolean {
    return TERMINAL.has(state);
}

type Boundary = Pick<Job, "state" | "publication_started" | "operation">;

/** Where the rewrite work stands. */
export function executionStatus(job: Boundary): StatusLine {
    switch (job.state) {
        case "Planning":
            return { label: "Planning", tone: "progress" };
        case "Queued":
            return { label: "Queued", tone: "progress" };
        case "Running":
            return { label: "Running", tone: "progress" };
        case "ReadyToCommit":
        case "Committing":
        case "CommitOutcomeUnknown":
        case "Succeeded":
            return { label: "Outputs ready", tone: "ok" };
        case "Deleting":
            return { label: "Deleting unreferenced files", tone: "progress" };
        case "Cancelling":
            return { label: "Stopping after cancellation", tone: "warning" };
        case "NoOp":
            return { label: "Nothing to rewrite", tone: "neutral" };
        case "Cancelled":
            return { label: "Stopped: cancelled", tone: "neutral" };
        case "ReplanRequired":
            return { label: "Stopped: replan required", tone: "error" };
        case "Failed":
            return { label: "Failed", tone: "error" };
    }
}

/** Whether the exact catalog publication happened. */
export function publicationStatus(job: Boundary): StatusLine {
    if (job.operation === "remove_orphan_files") {
        // Cleanup removes unreferenced objects and never commits, so there is
        // nothing to confirm or resolve.
        return { label: "Not applicable", tone: "neutral" };
    }
    switch (job.state) {
        case "Succeeded":
            return { label: "Confirmed", tone: "ok" };
        case "CommitOutcomeUnknown":
            return { label: "Outcome unknown", tone: "warning" };
        case "Committing":
            return { label: "Committing", tone: "progress" };
        case "NoOp":
            return { label: "Not needed", tone: "neutral" };
        case "Deleting":
            return { label: "Not applicable", tone: "neutral" };
        case "Cancelled":
        case "ReplanRequired":
        case "Failed":
            return { label: "Not published", tone: "neutral" };
        case "Planning":
        case "Queued":
        case "Running":
        case "ReadyToCommit":
        case "Cancelling":
            return job.publication_started
                ? { label: "Commit authorized", tone: "progress" }
                : { label: "Not started", tone: "neutral" };
    }
}

/** States that need an operator to understand more than "failed". */
export function notice(state: JobState): Notice | null {
    switch (state) {
        case "ReplanRequired":
            return {
                title: "Replan required",
                action: "The table changed in a way this plan cannot publish over, and its outputs cannot be reused. Retry to create a new job with a fresh plan.",
            };
        case "CommitOutcomeUnknown":
            return {
                title: "Commit outcome unknown",
                action: "The catalog did not confirm whether the commit applied. Bergsmith resolves it from publication evidence; do not start new maintenance on this table until this job leaves this state.",
            };
        default:
            return null;
    }
}

/** Cancellation is a request; it can be too late once publication begins. */
export function canCancel(job: Pick<Job, "state">): boolean {
    return !isTerminal(job.state);
}

/** Only a terminal job that did not succeed can be retried with a fresh plan. */
export function canRetry(job: Pick<Job, "state">): boolean {
    return isTerminal(job.state) && job.state !== "Succeeded";
}

/** What a cancellation request achieved. */
export function cancellationMessage(
    outcome: Schemas["CancelJobResult"]["cancellation"],
): StatusLine {
    switch (outcome) {
        case "cancelling":
            return {
                label: "Cancellation accepted; the job is stopping.",
                tone: "ok",
            };
        case "too_late_publication_started":
            return {
                label: "Too late: a commit was already authorized and may still publish.",
                tone: "warning",
            };
        case "publication_unresolved":
            return {
                label: "The commit outcome is unknown; cancellation cannot be assumed.",
                tone: "warning",
            };
        case "too_late_committed":
            return {
                label: "Too late: the job already published.",
                tone: "warning",
            };
        case "cancelled":
            return { label: "The job is already cancelled.", tone: "neutral" };
        case "already_ended":
            return {
                label: "The job already ended without publishing.",
                tone: "neutral",
            };
    }
}
