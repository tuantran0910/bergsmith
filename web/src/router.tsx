// A small path router: the application has six routes and needs no more.

import {
    type AnchorHTMLAttributes,
    type MouseEvent,
    useSyncExternalStore,
} from "react";

export type Route =
    | { name: "tables" }
    | { name: "table"; id: string }
    | { name: "jobs" }
    | { name: "job"; id: string }
    | { name: "audit" }
    | { name: "not_found" };

const listeners = new Set<() => void>();

function notify(): void {
    for (const listener of listeners) {
        listener();
    }
}

function subscribe(listener: () => void): () => void {
    listeners.add(listener);
    window.addEventListener("popstate", notify);
    return () => {
        listeners.delete(listener);
        if (listeners.size === 0) {
            window.removeEventListener("popstate", notify);
        }
    };
}

function currentPath(): string {
    return window.location.pathname;
}

/** Moves to `to` within the application. */
export function navigate(to: string): void {
    window.history.pushState(null, "", to);
    notify();
}

/** The current path, re-rendering when it changes. */
export function usePath(): string {
    return useSyncExternalStore(subscribe, currentPath);
}

const UUID = "([0-9a-fA-F-]{36})";

/** Maps a path to a route. */
export function match(path: string): Route {
    if (path === "/" || path === "/tables") {
        return { name: "tables" };
    }
    if (path === "/jobs") {
        return { name: "jobs" };
    }
    if (path === "/audit") {
        return { name: "audit" };
    }
    const table = new RegExp(`^/tables/${UUID}$`).exec(path);
    if (table?.[1] !== undefined) {
        return { name: "table", id: table[1] };
    }
    const job = new RegExp(`^/jobs/${UUID}$`).exec(path);
    if (job?.[1] !== undefined) {
        return { name: "job", id: job[1] };
    }
    return { name: "not_found" };
}

export function tablePath(id: string): string {
    return `/tables/${id}`;
}

export function jobPath(id: string): string {
    return `/jobs/${id}`;
}

type LinkProps = AnchorHTMLAttributes<HTMLAnchorElement> & { to: string };

/** An in-application link; modified clicks keep the browser's behavior. */
export function Link({ to, onClick, ...rest }: LinkProps) {
    const handle = (event: MouseEvent<HTMLAnchorElement>) => {
        onClick?.(event);
        if (
            event.defaultPrevented ||
            event.button !== 0 ||
            event.metaKey ||
            event.ctrlKey ||
            event.shiftKey ||
            event.altKey
        ) {
            return;
        }
        event.preventDefault();
        navigate(to);
    };
    return <a href={to} onClick={handle} {...rest} />;
}
