// Live job events over server-sent events (ADR 0036).
//
// The browser's `EventSource` resumes with `Last-Event-ID` after a dropped or
// expired stream. The server ends a terminal job's stream at once, so the
// client closes the source after a terminal event; otherwise it would
// reconnect forever.

import { useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useRef, useState } from "react";

import type { JobEvent } from "../api/client";
import { isTerminal } from "./status";

export const EVENT_KINDS: readonly JobEvent["kind"][] = [
    "created",
    "transitioned",
    "cancellation_requested",
    "publication_authorized",
];

/** `reconnecting` is the browser retrying after the stream dropped. */
export type Connection = "connecting" | "open" | "reconnecting" | "closed";

export function jobQueryKey(id: string): readonly ["job", string] {
    return ["job", id];
}

/** Parses one event's `data`, or `null` when it is not a job event. */
export function parseEvent(data: string): JobEvent | null {
    let value: unknown;
    try {
        value = JSON.parse(data);
    } catch {
        return null;
    }
    if (
        typeof value !== "object" ||
        value === null ||
        typeof (value as Record<string, unknown>)["sequence"] !== "number" ||
        typeof (value as Record<string, unknown>)["to_state"] !== "string"
    ) {
        return null;
    }
    return value as JobEvent;
}

/** Adds an event once, keeping sequence order. */
export function mergeEvent(
    events: readonly JobEvent[],
    event: JobEvent,
): JobEvent[] {
    if (events.some((existing) => existing.sequence === event.sequence)) {
        return [...events];
    }
    return [...events, event].sort((a, b) => a.sequence - b.sequence);
}

/** A refresh that can be requested often but runs at most one at a time. */
export interface Coalesced {
    (): void;
    /** Drops any pending refresh; later requests do nothing. */
    stop: () => void;
}

/**
 * Coalesces refresh requests. While one refresh runs, later requests collapse
 * into a single follow-up that starts after it settles, so a replay of many
 * events costs two reads, and the last read always starts after the newest
 * event arrived.
 */
export function coalesced(run: () => Promise<unknown>): Coalesced {
    let running = false;
    let pending = false;
    let stopped = false;
    const request = (() => {
        if (stopped) {
            return;
        }
        if (running) {
            pending = true;
            return;
        }
        running = true;
        void run()
            .catch(() => undefined)
            .finally(() => {
                running = false;
                if (pending) {
                    pending = false;
                    request();
                }
            });
    }) as Coalesced;
    request.stop = () => {
        stopped = true;
        pending = false;
    };
    return request;
}

export interface JobEvents {
    events: JobEvent[];
    connection: Connection;
    /** Opens a new stream after the last received event. */
    reconnect: () => void;
}

/**
 * Streams `jobId`'s events while the page is open. Each event refreshes the
 * job resource. `terminal` is the job's last known terminal flag: a stream
 * that fails for a terminal job is closed instead of retried.
 */
export function useJobEvents(jobId: string, terminal: boolean): JobEvents {
    const queryClient = useQueryClient();
    const [events, setEvents] = useState<JobEvent[]>([]);
    const [connection, setConnection] = useState<Connection>("connecting");
    const [generation, setGeneration] = useState(0);
    const source = useRef<EventSource | null>(null);
    const lastSequence = useRef(0);

    useEffect(() => {
        const after = lastSequence.current;
        const url = `/api/v1/jobs/${encodeURIComponent(jobId)}/events${after > 0 ? `?after=${after}` : ""}`;
        const stream = new EventSource(url);
        source.current = stream;
        const refresh = coalesced(() =>
            queryClient.refetchQueries({ queryKey: jobQueryKey(jobId) }),
        );
        const onEvent = (message: MessageEvent<string>) => {
            const event = parseEvent(message.data);
            if (event === null) {
                return;
            }
            lastSequence.current = Math.max(
                lastSequence.current,
                event.sequence,
            );
            setEvents((current) => mergeEvent(current, event));
            refresh();
            if (isTerminal(event.to_state)) {
                stream.close();
                setConnection("closed");
            }
        };
        for (const kind of EVENT_KINDS) {
            stream.addEventListener(kind, onEvent);
        }
        stream.onopen = () => {
            setConnection("open");
        };
        stream.onerror = () => {
            // Either the browser is reconnecting or it gave up (a non-200 answer).
            // Both re-read the job, which also surfaces an expired session.
            setConnection(
                stream.readyState === EventSource.CLOSED
                    ? "closed"
                    : "reconnecting",
            );
            refresh();
        };
        return () => {
            refresh.stop();
            stream.close();
            source.current = null;
        };
    }, [jobId, generation, queryClient]);

    // A terminal job's stream has nothing more to send: stop the browser from
    // reconnecting to it. The first connection still replays its history.
    useEffect(() => {
        if (
            terminal &&
            connection === "reconnecting" &&
            source.current !== null
        ) {
            source.current.close();
            setConnection("closed");
        }
    }, [terminal, connection]);

    const reconnect = useCallback(() => {
        setConnection("connecting");
        setGeneration((value) => value + 1);
    }, []);

    return { events, connection, reconnect };
}
