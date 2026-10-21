// Typed access to the `/api/v1` contract.
//
// Every request and response type comes from `schema.d.ts`, which is generated
// from the coordinator's OpenAPI document. Only the transport policy lives
// here: same-origin cookies, the CSRF echo, and one error type.

import createClient from "openapi-fetch";

import type { components, paths } from "./schema";

export type Schemas = components["schemas"];
export type ApiErrorBody = Schemas["ApiError"];
export type Job = Schemas["Job"];
export type JobState = Job["state"];
export type JobEvent = Schemas["JobEvent"];

/** The cookie whose value mutations echo in `x-csrf-token`. */
export const CSRF_COOKIE = "bergsmith_csrf";

/**
 * Untyped answers from a proxy in front of the coordinator: bad gateway,
 * unavailable, and gateway timeout. Repeating the same request is safe.
 */
const GATEWAY_FAILURES: ReadonlySet<number> = new Set([502, 503, 504]);

/** A failed request: an HTTP status and, when the API sent one, its typed body. */
export class ApiFailure extends Error {
    readonly status: number;
    readonly body: ApiErrorBody | null;

    constructor(status: number, body: ApiErrorBody | null) {
        super(
            body
                ? `${status} ${body.reason}`
                : `request failed with status ${status}`,
        );
        this.name = "ApiFailure";
        this.status = status;
        this.body = body;
    }

    /**
     * Whether the same request may be sent again unchanged: the API said so, or
     * the request never reached it.
     */
    get retryable(): boolean {
        return (
            this.status === 0 ||
            this.body?.disposition === "RetrySameRequest" ||
            (this.body === null && GATEWAY_FAILURES.has(this.status))
        );
    }

    /**
     * Whether a mutation may have taken effect anyway: no typed answer came
     * from the coordinator. A gateway can time out after the coordinator
     * committed, so the outcome of an untyped 5xx is unknown.
     */
    get outcomeUncertain(): boolean {
        return this.retryable || (this.body === null && this.status >= 500);
    }
}

/** Reads the CSRF value issued with the session, or an empty string. */
export function csrfToken(): string {
    for (const pair of document.cookie.split(";")) {
        const [name, ...value] = pair.trim().split("=");
        if (name === CSRF_COOKIE) {
            return value.join("=");
        }
    }
    return "";
}

/** The `x-csrf-token` header for a mutation. */
export function csrfHeader(): { "x-csrf-token": string } {
    return { "x-csrf-token": csrfToken() };
}

/** A fresh idempotency key for one user action. */
export function idempotencyKey(): string {
    return crypto.randomUUID();
}

export const api = createClient<paths>({
    // Same origin as the page; an absolute base also satisfies runtimes whose
    // `Request` does not resolve relative URLs.
    baseUrl: window.location.origin,
    credentials: "same-origin",
    // Resolved per request so tests and the browser use the current `fetch`.
    fetch: (request: Request) => globalThis.fetch(request),
});

function isApiErrorBody(value: unknown): value is ApiErrorBody {
    return (
        typeof value === "object" &&
        value !== null &&
        typeof (value as Record<string, unknown>)["reason"] === "string" &&
        typeof (value as Record<string, unknown>)["operator_action"] ===
            "string"
    );
}

/**
 * Resolves an `openapi-fetch` call to its data, or throws {@link ApiFailure}.
 * A network failure becomes status 0, which is retryable.
 */
export async function call<T>(
    request: Promise<{ data?: T; error?: unknown; response: Response }>,
): Promise<T> {
    let result: { data?: T; error?: unknown; response: Response };
    try {
        result = await request;
    } catch {
        throw new ApiFailure(0, null);
    }
    if (!result.response.ok || result.data === undefined) {
        throw new ApiFailure(
            result.response.status,
            isApiErrorBody(result.error) ? result.error : null,
        );
    }
    return result.data;
}
