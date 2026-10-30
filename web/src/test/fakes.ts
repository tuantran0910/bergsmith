// Test doubles for the coordinator: a routed `fetch` whose bodies are typed by
// the generated schema, and a controllable `EventSource`.

import type { Schemas } from "../api/client";
import type { paths } from "../api/schema";

/** Returns `value` checked against a generated schema at compile time. */
export function schema<K extends keyof Schemas>(
    _name: K,
    value: Schemas[K],
): Schemas[K] {
    return value;
}

export function json(
    status: number,
    body: unknown,
    headers: Record<string, string> = {},
): Response {
    return new Response(JSON.stringify(body), {
        status,
        headers: { "content-type": "application/json", ...headers },
    });
}

/** A typed API error body. */
export function apiError(
    status: number,
    reason: Schemas["ApiError"]["reason"],
    disposition: Schemas["ApiError"]["disposition"],
    operatorAction: string,
): Response {
    return json(
        status,
        schema("ApiError", {
            reason,
            disposition,
            operator_action: operatorAction,
        }),
    );
}

type Method = "GET" | "POST" | "PATCH" | "DELETE";
type RoutePath = keyof paths | "/auth/session" | "/auth/logout";
type Handler = (
    request: Request,
    params: Record<string, string>,
) => Response | Promise<Response>;

interface RouteEntry {
    method: Method;
    pattern: RegExp;
    names: string[];
    handlers: Handler[];
}

/** A fake coordinator behind `fetch`. Unrouted requests fail the test. */
export class FakeApi {
    readonly requests: Request[] = [];
    readonly unrouted: string[] = [];
    private readonly routes: RouteEntry[] = [];

    constructor() {
        // Filtered lists read facet counts and saved views beside each page
        // (ADR 0067); a test that asserts them routes its own, which wins.
        this.on("GET", "/api/v1/tables/facets", () =>
            json(200, schema("TableFacets", { health: [], catalog: [] })),
        )
            .on("GET", "/api/v1/jobs/facets", () =>
                json(200, schema("JobFacets", { state: [], operation: [] })),
            )
            .on("GET", "/api/v1/catalogs/facets", () =>
                json(200, schema("CatalogFacets", { enabled: [] })),
            )
            .on("GET", "/api/v1/audit/facets", () =>
                json(
                    200,
                    schema("AuditFacets", {
                        outcome: [],
                        action: [],
                        resource_type: [],
                    }),
                ),
            )
            .on("GET", "/api/v1/principals/facets", () =>
                json(200, schema("PrincipalFacets", { kind: [], active: [] })),
            )
            .on("GET", "/api/v1/me/views", () =>
                json(200, schema("SavedViewList", { items: [] })),
            );
    }

    /**
     * Routes `method path`. Several handlers answer successive requests; the
     * last one repeats.
     */
    on(method: Method, path: RoutePath, ...handlers: Handler[]): this {
        const names: string[] = [];
        const source = path.replace(/\{(\w+)\}/g, (_match, name: string) => {
            names.push(name);
            return "([^/]+)";
        });
        this.routes.unshift({
            method,
            pattern: new RegExp(`^${source}$`),
            names,
            handlers,
        });
        return this;
    }

    /** Requests to `method path`, in order. */
    sent(method: Method, pathname: string): Request[] {
        return this.requests.filter(
            (request) =>
                request.method === method &&
                new URL(request.url).pathname === pathname,
        );
    }

    readonly fetch = async (
        input: RequestInfo | URL,
        init?: RequestInit,
    ): Promise<Response> => {
        const request =
            input instanceof Request
                ? input
                : new Request(
                      new URL(String(input), window.location.href),
                      init,
                  );
        this.requests.push(request.clone());
        const pathname = new URL(request.url).pathname;
        // Like the coordinator's router, a literal path such as
        // `/tables/facets` wins over a parameter such as `/tables/{id}`.
        const ordered = [
            ...this.routes.filter((route) => route.names.length === 0),
            ...this.routes.filter((route) => route.names.length > 0),
        ];
        for (const route of ordered) {
            const found =
                route.method === request.method
                    ? route.pattern.exec(pathname)
                    : null;
            if (found) {
                const params = Object.fromEntries(
                    route.names.map((name, index) => [
                        name,
                        found[index + 1] ?? "",
                    ]),
                );
                const handler =
                    route.handlers.length > 1
                        ? route.handlers.shift()
                        : route.handlers[0];
                if (handler) {
                    return handler(request, params);
                }
            }
        }
        this.unrouted.push(`${request.method} ${pathname}`);
        return json(599, { error: "unrouted" });
    };
}

type Listener = (event: MessageEvent<string>) => void;

/** A controllable stand-in for the browser's `EventSource`. */
export class FakeEventSource {
    static readonly CONNECTING = 0;
    static readonly OPEN = 1;
    static readonly CLOSED = 2;
    static instances: FakeEventSource[] = [];

    readonly url: string;
    readyState = FakeEventSource.CONNECTING;
    onopen: ((event: Event) => void) | null = null;
    onerror: ((event: Event) => void) | null = null;
    private readonly listeners = new Map<string, Listener[]>();

    constructor(url: string) {
        this.url = url;
        FakeEventSource.instances.push(this);
    }

    static latest(): FakeEventSource {
        const latest = FakeEventSource.instances.at(-1);
        if (latest === undefined) {
            throw new Error("no EventSource was opened");
        }
        return latest;
    }

    addEventListener(type: string, listener: Listener): void {
        this.listeners.set(type, [
            ...(this.listeners.get(type) ?? []),
            listener,
        ]);
    }

    close(): void {
        this.readyState = FakeEventSource.CLOSED;
    }

    open(): void {
        this.readyState = FakeEventSource.OPEN;
        this.onopen?.(new Event("open"));
    }

    emit(event: Schemas["JobEvent"]): void {
        const message = new MessageEvent<string>(event.kind, {
            data: JSON.stringify(event),
            lastEventId: String(event.sequence),
        });
        for (const listener of this.listeners.get(event.kind) ?? []) {
            listener(message);
        }
    }

    /** The stream dropped and the browser is reconnecting. */
    drop(): void {
        this.readyState = FakeEventSource.CONNECTING;
        this.onerror?.(new Event("error"));
    }

    /** The browser gave up, as after a non-200 answer. */
    fail(): void {
        this.readyState = FakeEventSource.CLOSED;
        this.onerror?.(new Event("error"));
    }
}
