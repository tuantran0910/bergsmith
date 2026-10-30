// Live list updates over the organization's job stream (ADR 0067).
//
// The shell holds one `GET /api/v1/events` stream per tab. A `job` event names
// a job, its table, its state, and its revision; the client finds the cached
// reads that show that job and refetches only those, in batches, so a burst of
// transitions costs one read per affected query rather than one per event.
// The stream closes while the tab is hidden and reopens after the last ID it
// saw; when it cannot be held, the same reads are polled instead.

import {
    type InfiniteData,
    type Query,
    type QueryClient,
    useQueryClient,
} from "@tanstack/react-query";
import { useEffect } from "react";

import type { Job, Schemas } from "./api/client";
import { isTerminal } from "./jobs/status";

export type LiveJobEvent = Schemas["LiveJobEvent"];

/** The organization stream's path. */
export const LIVE_EVENTS_PATH = "/api/v1/events";

/** How long events gather before the affected reads refetch. */
export const LIVE_BATCH_MS = 250;

/** How often reads are polled while the stream cannot be held. */
export const LIVE_FALLBACK_MS = 30_000;

/** Query roots that show job state; a reload refreshes these. */
const LIVE_ROOTS: ReadonlySet<unknown> = new Set([
    "overview",
    "jobs",
    "job",
    "table",
]);

/** Parses a `job` event's data, or `null` when it is not one. */
export function parseLiveEvent(data: string): LiveJobEvent | null {
    let value: unknown;
    try {
        value = JSON.parse(data);
    } catch {
        return null;
    }
    if (typeof value !== "object" || value === null) {
        return null;
    }
    const record = value as Record<string, unknown>;
    if (
        typeof record["job_id"] !== "string" ||
        typeof record["table_id"] !== "string" ||
        typeof record["state"] !== "string" ||
        typeof record["revision"] !== "number"
    ) {
        return null;
    }
    return value as LiveJobEvent;
}

/** Whether a cached job already shows `event` or something newer. */
function shows(job: Job | undefined, event: LiveJobEvent): boolean {
    return (
        job !== undefined &&
        job.id === event.job_id &&
        job.revision >= event.revision
    );
}

/**
 * Whether the cached read under `key` may show something `event` changed. A
 * job list that already holds the job at this revision or later is current;
 * any other list may gain, lose, or reorder it, and counts always move.
 */
export function isAffected(
    key: readonly unknown[],
    data: unknown,
    event: LiveJobEvent,
): boolean {
    const [root, second, third] = key;
    switch (root) {
        case "overview":
            return true;
        case "jobs":
            if (second === "facets") {
                return true;
            }
            if (second === "list") {
                const pages = (
                    data as InfiniteData<{ items: Job[] }> | undefined
                )?.pages;
                return !(pages ?? []).some((page) =>
                    page.items.some((job) => shows(job, event)),
                );
            }
            return second === event.job_id && third === "tasks";
        case "job":
            return (
                second === event.job_id &&
                !shows(data as Job | undefined, event)
            );
        case "table":
            // A table's history and totals change when one of its jobs ends.
            return second === event.table_id && isTerminal(event.state);
        default:
            return false;
    }
}

/** Whether `key` is one of the reads a reload refreshes. */
export function isLive(key: readonly unknown[]): boolean {
    return LIVE_ROOTS.has(key[0]);
}

/**
 * One tab's organization stream. Affected queries are collected by hash and
 * refetched every {@link LIVE_BATCH_MS}; a query whose fetch is still running
 * waits for the next batch, because that fetch may have started before the
 * event.
 */
export class LiveUpdates {
    private source: EventSource | null = null;
    private lastId: string | null = null;
    private readonly dirty = new Set<string>();
    private batch: ReturnType<typeof setTimeout> | null = null;
    private fallback: ReturnType<typeof setTimeout> | null = null;
    private stopped = false;

    constructor(private readonly client: QueryClient) {}

    /** Opens the stream when the tab is visible, and follows visibility. */
    start(): void {
        document.addEventListener("visibilitychange", this.onVisibility);
        if (!document.hidden) {
            this.open();
        }
    }

    /** Closes the stream and drops pending work. */
    stop(): void {
        this.stopped = true;
        document.removeEventListener("visibilitychange", this.onVisibility);
        this.close();
        if (this.batch !== null) {
            clearTimeout(this.batch);
            this.batch = null;
        }
        this.dirty.clear();
    }

    private readonly onVisibility = () => {
        if (document.hidden) {
            this.close();
        } else if (this.source === null) {
            this.open();
        }
    };

    private open(): void {
        if (this.stopped) {
            return;
        }
        this.clearFallback();
        // A new source cannot set `Last-Event-ID`; `after` carries the same
        // resume point. The browser's own reconnects send the header.
        const url =
            this.lastId === null
                ? LIVE_EVENTS_PATH
                : `${LIVE_EVENTS_PATH}?after=${encodeURIComponent(this.lastId)}`;
        const source = new EventSource(url);
        this.source = source;
        source.addEventListener("ready", (message: MessageEvent<string>) => {
            // Without a resume point the stream starts at the first settled
            // horizon, so what the page read before it may already be
            // behind. A resumed stream replays instead.
            const resumed = this.lastId !== null;
            this.track(message.lastEventId);
            if (!resumed) {
                this.reload();
            }
        });
        source.addEventListener("reset", (message: MessageEvent<string>) => {
            this.track(message.lastEventId);
            this.reload();
        });
        source.addEventListener("job", (message: MessageEvent<string>) => {
            this.track(message.lastEventId);
            const event = parseLiveEvent(message.data);
            if (event !== null) {
                this.receive(event);
            }
        });
        source.onerror = () => {
            // The browser retries a dropped stream by itself; a refused one
            // (capacity, an ended session) is closed and falls back to polling.
            if (source.readyState === EventSource.CLOSED) {
                this.source = null;
                this.poll();
            }
        };
    }

    private close(): void {
        this.clearFallback();
        this.source?.close();
        this.source = null;
    }

    private poll(): void {
        if (this.stopped || document.hidden) {
            return;
        }
        this.fallback = setTimeout(() => {
            this.fallback = null;
            this.reload();
            this.open();
        }, LIVE_FALLBACK_MS);
    }

    private clearFallback(): void {
        if (this.fallback !== null) {
            clearTimeout(this.fallback);
            this.fallback = null;
        }
    }

    private track(id: string): void {
        if (id !== "") {
            this.lastId = id;
        }
    }

    private receive(event: LiveJobEvent): void {
        for (const query of this.client.getQueryCache().getAll()) {
            if (isAffected(query.queryKey, query.state.data, event)) {
                this.dirty.add(query.queryHash);
            }
        }
        this.schedule();
    }

    private reload(): void {
        for (const query of this.client.getQueryCache().getAll()) {
            if (isLive(query.queryKey)) {
                this.dirty.add(query.queryHash);
            }
        }
        this.schedule();
    }

    private schedule(): void {
        if (this.batch === null && this.dirty.size > 0 && !this.stopped) {
            this.batch = setTimeout(this.flush, LIVE_BATCH_MS);
        }
    }

    private readonly flush = () => {
        this.batch = null;
        const cache = this.client.getQueryCache();
        for (const hash of [...this.dirty]) {
            const query: Query | undefined = cache.get(hash);
            if (query === undefined) {
                this.dirty.delete(hash);
            } else if (query.state.fetchStatus !== "fetching") {
                this.dirty.delete(hash);
                void this.client.invalidateQueries({
                    queryKey: query.queryKey,
                    exact: true,
                });
            }
        }
        this.schedule();
    };
}

/**
 * Keeps the session's cached job reads current while the shell is shown.
 * One stream serves every page of the tab.
 */
export function useLiveUpdates(): void {
    const client = useQueryClient();
    useEffect(() => {
        const live = new LiveUpdates(client);
        live.start();
        return () => {
            live.stop();
        };
    }, [client]);
}
