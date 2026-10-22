import { canManageWorkers, canReadAudit, loginUrl } from "./api/auth";
import { ApiFailure, call, csrfToken } from "./api/client";
import {
    aliasName,
    bytes,
    duration,
    shortId,
    timestamp,
    words,
} from "./format";
import { coalesced, mergeEvent, parseEvent } from "./jobs/events";
import { binKey } from "./pages/PreviewView";
import { match } from "./router";
import { event, session } from "./test/fixtures";

describe("router", () => {
    const id = "0190f3c4-0000-7000-8000-000000000001";

    it.each([
        ["/", { name: "tables" }],
        ["/tables", { name: "tables" }],
        [`/tables/${id}`, { name: "table", id }],
        ["/jobs", { name: "jobs" }],
        [`/jobs/${id}`, { name: "job", id }],
        ["/audit", { name: "audit" }],
        ["/workers", { name: "workers" }],
        ["/jobs/not-a-uuid", { name: "not_found" }],
        [`/jobs/${id}/extra`, { name: "not_found" }],
        ["/schedules", { name: "not_found" }],
    ])("matches %s", (path, route) => {
        expect(match(path)).toEqual(route);
    });
});

describe("format", () => {
    it.each([
        [0, "0 B"],
        [1023, "1023 B"],
        [1536, "1.5 KiB"],
        [5 * 1024 ** 3, "5.0 GiB"],
        [2 * 1024 ** 6, "2048.0 PiB"],
    ])("formats %d bytes", (value, text) => {
        expect(bytes(value)).toBe(text);
    });

    it.each([
        [900, "1 s"],
        [61_000, "1 min 1 s"],
        [3_720_000, "1 h 2 min"],
    ])("formats %d ms", (value, text) => {
        expect(duration(value)).toBe(text);
    });

    it("keeps an unparseable timestamp as given", () => {
        expect(timestamp("not a time")).toBe("not a time");
        expect(timestamp("2026-10-21T10:00:00Z")).not.toBe(
            "2026-10-21T10:00:00Z",
        );
    });

    it("turns codes into words and shortens IDs", () => {
        expect(words("retry_same_request")).toBe("Retry same request");
        expect(shortId("0190f3c4-0000")).toBe("0190f3c4");
        expect(
            aliasName(
                { catalog_id: "c", namespace: ["a", "b"], name: "t" },
                new Map([["c", "lake"]]),
            ),
        ).toBe("lake.a.b.t");
    });
});

describe("events", () => {
    it("parses only job events", () => {
        expect(
            parseEvent(JSON.stringify(event(1, "Planning", null)))?.sequence,
        ).toBe(1);
        expect(parseEvent("not json")).toBeNull();
        expect(parseEvent("null")).toBeNull();
        expect(parseEvent('{"sequence":"1","to_state":"Planning"}')).toBeNull();
    });

    it("merges events once in sequence order", () => {
        const merged = mergeEvent(
            [event(1, "Planning", null), event(3, "Running", "Queued")],
            event(2, "Queued", "Planning"),
        );
        expect(merged.map((item) => item.sequence)).toEqual([1, 2, 3]);
        expect(mergeEvent(merged, event(2, "Queued", "Planning"))).toHaveLength(
            3,
        );
    });
});

describe("coalesced refresh", () => {
    it("runs one refresh at a time and one more after the newest request", async () => {
        const settle: (() => void)[] = [];
        let runs = 0;
        const refresh = coalesced(() => {
            runs += 1;
            return new Promise<void>((resolve) => settle.push(resolve));
        });

        for (let index = 0; index < 100; index += 1) {
            refresh();
        }
        expect(runs).toBe(1);
        settle.shift()?.();
        await vi.waitFor(() => {
            expect(runs).toBe(2);
        });
        settle.shift()?.();
        await Promise.resolve();
        await Promise.resolve();
        expect(runs).toBe(2);
    });

    it("survives a failed refresh and stops when asked", async () => {
        let runs = 0;
        const refresh = coalesced(() => {
            runs += 1;
            return Promise.reject(new Error("offline"));
        });
        refresh();
        refresh();
        await vi.waitFor(() => {
            expect(runs).toBe(2);
        });
        refresh.stop();
        refresh();
        expect(runs).toBe(2);
    });
});

describe("bin keys", () => {
    it("tell apart bins whose ordinals restart in each partition", () => {
        const bin = (value: string, ordinal: number) => ({
            partition: { spec_id: 0, values: [{ field_id: 1, value }] },
            ordinal,
            input: { files: 2, bytes: 2 },
            has_deletes: false,
            files_removed: 1,
            disposition: "admitted" as const,
        });
        expect(binKey(bin("a", 0))).not.toBe(binKey(bin("b", 0)));
        expect(binKey(bin("a", 0))).not.toBe(binKey(bin("a", 1)));
        expect(binKey(bin("a", 0))).toBe(binKey(bin("a", 0)));
    });
});

describe("client", () => {
    afterEach(() => {
        document.cookie = "other=; max-age=0";
    });

    it("reads the CSRF cookie among others", () => {
        expect(csrfToken()).toBe("");
        document.cookie = "other=1";
        document.cookie = "bergsmith_csrf=a=b";
        expect(csrfToken()).toBe("a=b");
    });

    it("classifies failures", async () => {
        await expect(
            call(Promise.reject(new TypeError("offline"))),
        ).rejects.toMatchObject({
            status: 0,
            retryable: true,
        });
        const notJson = call(
            Promise.resolve({
                error: "text",
                response: new Response("x", { status: 418 }),
            }),
        );
        await expect(notJson).rejects.toMatchObject({
            status: 418,
            body: null,
            retryable: false,
            outcomeUncertain: false,
        });
        const permanent = new ApiFailure(409, {
            reason: "job_not_terminal",
            disposition: "PermanentFailure",
            operator_action: "wait_for_job",
        });
        expect(permanent.retryable).toBe(false);
        expect(permanent.outcomeUncertain).toBe(false);
        expect(permanent.message).toBe("409 job_not_terminal");
    });

    it.each([
        // status, typed body, retryable, outcome uncertain
        [0, false, true, true],
        [502, false, true, true],
        [503, false, true, true],
        [504, false, true, true],
        [500, false, false, true],
        [500, true, false, false],
        [503, true, true, true],
        [400, false, false, false],
    ])(
        "status %d (typed: %s) is retryable %s, uncertain %s",
        (status, typed, retryable, uncertain) => {
            const failure = new ApiFailure(
                status,
                typed
                    ? {
                          reason:
                              status === 503
                                  ? "ledger_unavailable"
                                  : "internal_error",
                          disposition:
                              status === 503
                                  ? "RetrySameRequest"
                                  : "PermanentFailure",
                          operator_action: "contact_operator",
                      }
                    : null,
            );
            expect(failure.retryable).toBe(retryable);
            expect(failure.outcomeUncertain).toBe(uncertain);
        },
    );
});

describe("auth", () => {
    it("builds the login URL", () => {
        expect(loginUrl("acme corp", "/jobs?x=1")).toBe(
            "/auth/login?organization=acme+corp&return_to=%2Fjobs%3Fx%3D1",
        );
    });

    it("shows workers only to organization-wide administrators", () => {
        expect(canManageWorkers(session("Administrator"))).toBe(true);
        expect(canManageWorkers(session("Operator"))).toBe(false);
    });

    it("shows audit only to organization-wide administrators", () => {
        expect(canReadAudit(session("Administrator"))).toBe(true);
        expect(canReadAudit(session("Operator"))).toBe(false);
        expect(
            canReadAudit({
                ...session(),
                grants: [
                    {
                        role: "Administrator",
                        scope: { catalog_connection: "c", table: null },
                    },
                ],
            }),
        ).toBe(false);
    });
});
