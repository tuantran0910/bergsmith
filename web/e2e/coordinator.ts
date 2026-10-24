// The browser gate's coordinator: every request the page makes is answered
// here, so a run needs no server and no network. The built UI is served from
// `dist/` with the coordinator's fallback rules and security headers, read
// from its source so the gate checks the policy that ships; `/auth` and
// `/api/v1` answer from contract-typed fixtures.

import { readFile, stat } from "node:fs/promises";
import path from "node:path";

import AxeBuilder from "@axe-core/playwright";
import { test as base, expect, type Page, type Route } from "@playwright/test";

import type { JobEvent } from "../src/api/client";
import type { paths } from "../src/api/schema";

/** A secure origin, so the page has `crypto.randomUUID` and the Clipboard API. */
export const ORIGIN = "https://bergsmith.test";

const WEB = path.resolve(import.meta.dirname, "..");
const DIST = path.join(WEB, "dist");
const HTTP_SOURCE = path.join(WEB, "../crates/coordinator/src/runtime/http.rs");

/** Reads `CONTENT_SECURITY_POLICY` from the coordinator's source. */
export async function contentSecurityPolicy(): Promise<string> {
    const source = await readFile(HTTP_SOURCE, "utf8");
    const found =
        /const CONTENT_SECURITY_POLICY: &str = "((?:[^"\\]|\\\n)*)";/.exec(
            source,
        );
    if (found?.[1] === undefined) {
        throw new Error(`CONTENT_SECURITY_POLICY not found in ${HTTP_SOURCE}`);
    }
    // A Rust string continuation drops the newline and the next line's indent.
    return found[1].replace(/\\\n\s*/g, "");
}

/** The headers the coordinator adds to every response. */
async function securityHeaders(): Promise<Record<string, string>> {
    return {
        "content-security-policy": await contentSecurityPolicy(),
        "x-content-type-options": "nosniff",
        "referrer-policy": "no-referrer",
        "x-frame-options": "DENY",
        "permissions-policy": "camera=(), microphone=(), geolocation=()",
    };
}

const CONTENT_TYPES: Record<string, string> = {
    ".html": "text/html; charset=utf-8",
    ".js": "text/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
    ".svg": "image/svg+xml",
    ".png": "image/png",
    ".ico": "image/x-icon",
    ".woff2": "font/woff2",
};

/** A reply from the fake API. */
export interface Reply {
    status: number;
    body: string;
    contentType: string;
    headers?: Record<string, string>;
}

export function json(
    status: number,
    body: unknown,
    headers: Record<string, string> = {},
): Reply {
    return {
        status,
        body: JSON.stringify(body),
        contentType: "application/json",
        headers,
    };
}

/**
 * A finished event stream. The long `retry` keeps the browser from
 * reconnecting during a test after the body ends.
 */
export function eventStream(events: readonly JobEvent[]): Reply {
    const body = events
        .map(
            (event) =>
                `id: ${event.sequence}\nevent: ${event.kind}\ndata: ${JSON.stringify(event)}\n\n`,
        )
        .join("");
    return {
        status: 200,
        body: `retry: 3600000\n\n${body}`,
        contentType: "text/event-stream",
    };
}

/** A request the fake API answered, for assertions. */
export interface Sent {
    method: string;
    pathname: string;
    search: string;
    headers: Record<string, string>;
    body: unknown;
}

type Method = "GET" | "POST" | "PATCH" | "DELETE";
type RoutePath =
    | keyof paths
    | "/api/v1/jobs/{id}/events"
    | "/auth/session"
    | "/auth/logout"
    | "/auth/login";
type Handler = (sent: Sent, params: Record<string, string>) => Reply;

interface RouteEntry {
    method: Method;
    pattern: RegExp;
    names: string[];
    handlers: Handler[];
}

/** Answers one page's requests; anything it cannot answer fails the test. */
export class FakeCoordinator {
    readonly requests: Sent[] = [];
    readonly unrouted: string[] = [];
    private readonly routes: RouteEntry[] = [];
    private headers: Record<string, string> = {};

    /**
     * Routes `method path`; later routes take precedence. Several handlers
     * answer successive requests; the last one repeats.
     */
    on(method: Method, routePath: RoutePath, ...handlers: Handler[]): this {
        const names: string[] = [];
        const source = routePath.replace(
            /\{(\w+)\}/g,
            (_match, name: string) => {
                names.push(name);
                return "([^/]+)";
            },
        );
        this.routes.unshift({
            method,
            pattern: new RegExp(`^${source}$`),
            names,
            handlers,
        });
        return this;
    }

    /** Requests to `method pathname`, in order. */
    sent(method: Method, pathname: string): Sent[] {
        return this.requests.filter(
            (request) =>
                request.method === method && request.pathname === pathname,
        );
    }

    async install(page: Page): Promise<void> {
        this.headers = await securityHeaders();
        await page.route("**/*", (route) => this.handle(route));
    }

    private async handle(route: Route): Promise<void> {
        const request = route.request();
        const url = new URL(request.url());
        if (url.origin !== ORIGIN) {
            // The UI must work without any other host (ADR 0046, BYOC).
            this.unrouted.push(`${request.method()} ${url.href}`);
            await route.abort("blockedbyclient");
            return;
        }
        const isApi = /^\/(api|auth)(\/|$)/.test(url.pathname);
        const reply = isApi
            ? this.answer({
                  method: request.method(),
                  pathname: url.pathname,
                  search: url.search,
                  headers: await request.allHeaders(),
                  body: parseBody(request.postData()),
              })
            : await serveStatic(url.pathname);
        await route.fulfill({
            status: reply.status,
            contentType: reply.contentType,
            headers: { ...this.headers, ...reply.headers },
            body: reply.body,
        });
    }

    private answer(sent: Sent): Reply {
        this.requests.push(sent);
        for (const route of this.routes) {
            const found =
                route.method === sent.method
                    ? route.pattern.exec(sent.pathname)
                    : null;
            if (found) {
                const params = Object.fromEntries(
                    route.names.map((name, index) => [
                        name,
                        decodeURIComponent(found[index + 1] ?? ""),
                    ]),
                );
                const handler =
                    route.handlers.length > 1
                        ? route.handlers.shift()
                        : route.handlers[0];
                if (handler) {
                    return handler(sent, params);
                }
            }
        }
        this.unrouted.push(`${sent.method} ${sent.pathname}`);
        return json(599, { error: "unrouted" });
    }
}

function parseBody(data: string | null): unknown {
    if (data === null) {
        return null;
    }
    try {
        return JSON.parse(data) as unknown;
    } catch {
        return data;
    }
}

/** The coordinator's static serving (`serve_static` in `http.rs`). */
async function serveStatic(pathname: string): Promise<Reply> {
    const relative = path.posix.normalize(pathname).replace(/^\/+/, "");
    if (relative.startsWith("..")) {
        return notFound();
    }
    let file = path.join(DIST, relative === "" ? "index.html" : relative);
    const found = await stat(file).catch(() => null);
    if (found === null || !found.isFile()) {
        if (path.posix.extname(relative) !== "") {
            return notFound();
        }
        file = path.join(DIST, "index.html");
    }
    const immutable =
        relative.startsWith("assets/") && relative !== "index.html";
    return {
        status: 200,
        body: await readFile(file, "utf8"),
        contentType:
            CONTENT_TYPES[path.extname(file)] ?? "application/octet-stream",
        headers: {
            "cache-control": immutable
                ? "public, max-age=31536000, immutable"
                : "no-cache",
        },
    };
}

function notFound(): Reply {
    return { status: 404, body: "", contentType: "text/plain" };
}

/**
 * Chromium reports each non-2xx response on the console. The fake answers
 * `401` and typed errors on purpose; the page's handling is what is tested.
 */
const EXPECTED_CONSOLE =
    /^Failed to load resource: the server responded with a status of \d+/;

interface Fixtures {
    coordinator: FakeCoordinator;
    /** Console errors, page errors, and policy violations seen so far. */
    problems: string[];
}

/**
 * Tests that run against the fake coordinator. Each one fails on a CSP or
 * Trusted Types violation, a console error, an uncaught page error, or a
 * request the fake does not answer.
 */
// Playwright names a fixture's continuation `use`; React's hook lint would
// mistake that for the `use` hook, so it is called `provide` here.
export const test = base.extend<Fixtures>({
    // Automatic, so every test listens from its first navigation.
    problems: [
        async ({ page }, provide) => {
            const problems: string[] = [];
            await page.exposeBinding("__reportViolation", (_source, text) => {
                problems.push(`policy violation: ${String(text)}`);
            });
            await page.addInitScript(() => {
                document.addEventListener(
                    "securitypolicyviolation",
                    (event) => {
                        const report = (
                            window as unknown as {
                                __reportViolation: (text: string) => void;
                            }
                        ).__reportViolation;
                        report(
                            `${event.effectiveDirective} blocked ${event.blockedURI || "inline"} (${event.sample})`,
                        );
                    },
                );
            });
            page.on("console", (message) => {
                if (
                    message.type() === "error" &&
                    !EXPECTED_CONSOLE.test(message.text())
                ) {
                    problems.push(`console: ${message.text()}`);
                }
            });
            page.on("pageerror", (error) => {
                problems.push(`page error: ${error.message}`);
            });
            await provide(problems);
            expect(problems, "browser problems").toEqual([]);
        },
        { auto: true },
    ],
    coordinator: async ({ page, context }, provide) => {
        await context.addCookies([
            {
                name: "bergsmith_csrf",
                value: "csrf-value",
                url: ORIGIN,
                secure: true,
                sameSite: "Strict",
            },
        ]);
        const coordinator = new FakeCoordinator();
        await coordinator.install(page);
        await provide(coordinator);
        expect(coordinator.unrouted, "unrouted requests").toEqual([]);
    },
});

export { expect };

/** WCAG 2.0, 2.1, and 2.2 at levels A and AA. */
const WCAG_TAGS = [
    "wcag2a",
    "wcag2aa",
    "wcag21a",
    "wcag21aa",
    "wcag22a",
    "wcag22aa",
];

/** Fails on any WCAG 2.2 AA violation axe finds on the page as shown. */
export async function expectAccessible(page: Page): Promise<void> {
    // Opening dialogs and hover transitions temporarily blend text with the
    // backdrop. Measure the controls after finite animations have settled;
    // perpetual loading indicators must never hold up the accessibility gate.
    await page.evaluate(async () => {
        await Promise.all(
            document
                .getAnimations()
                .filter(
                    (animation) =>
                        animation.effect?.getComputedTiming().iterations !==
                        Infinity,
                )
                .map((animation) => animation.finished.catch(() => undefined)),
        );
    });
    const results = await new AxeBuilder({ page })
        .withTags(WCAG_TAGS)
        .analyze();
    // An axe that could not inspect the page reports no violations either.
    expect(results.passes.length, "axe rules that ran").toBeGreaterThan(0);
    expect(
        results.violations.map(
            (violation) =>
                `${violation.id}: ${violation.nodes
                    .map(
                        (node) =>
                            `${node.target.join(" ")}: ${node.failureSummary ?? ""}`,
                    )
                    .join(", ")}`,
        ),
        "accessibility violations",
    ).toEqual([]);
}
