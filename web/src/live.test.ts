import { QueryClient } from "@tanstack/react-query";

import type { Job } from "./api/client";
import {
    LIVE_BATCH_MS,
    LIVE_FALLBACK_MS,
    LiveUpdates,
    isAffected,
    isLive,
    parseLiveEvent,
    type LiveJobEvent,
} from "./live";
import { FakeEventSource } from "./test/fakes";
import { job } from "./test/fixtures";

const JOB = "0190a000-0000-7000-8000-000000000001";
const OTHER_JOB = "0190a000-0000-7000-8000-000000000002";
const TABLE = "0190a000-0000-7000-8000-0000000000aa";
const OTHER_TABLE = "0190a000-0000-7000-8000-0000000000bb";

function event(patch: Partial<LiveJobEvent> = {}): LiveJobEvent {
    return {
        job_id: JOB,
        table_id: TABLE,
        state: "Running",
        revision: 3,
        ...patch,
    };
}

function listed(revision: number): { pages: { items: Job[] }[] } {
    return {
        pages: [
            {
                items: [{ ...job("Running"), id: JOB, revision }],
            },
        ],
    };
}

describe("parseLiveEvent", () => {
    it("accepts a job event's data", () => {
        expect(parseLiveEvent(JSON.stringify(event()))).toEqual(event());
    });

    it.each(["not json", "null", "{}", '{"job_id":1}'])(
        "rejects %s",
        (data) => {
            expect(parseLiveEvent(data)).toBeNull();
        },
    );
});

describe("isAffected", () => {
    it("refreshes counts and the overview for any event", () => {
        expect(isAffected(["overview"], undefined, event())).toBe(true);
        expect(isAffected(["jobs", "facets", {}], undefined, event())).toBe(
            true,
        );
    });

    it("leaves a list that already shows the job at that revision", () => {
        expect(isAffected(["jobs", "list", {}], listed(3), event())).toBe(
            false,
        );
        expect(isAffected(["jobs", "list", {}], listed(4), event())).toBe(
            false,
        );
    });

    it("refreshes a list that shows an older revision or lacks the job", () => {
        expect(isAffected(["jobs", "list", {}], listed(2), event())).toBe(true);
        expect(
            isAffected(
                ["jobs", "list", {}],
                listed(3),
                event({ job_id: OTHER_JOB }),
            ),
        ).toBe(true);
        expect(isAffected(["jobs", "list", {}], undefined, event())).toBe(true);
    });

    it("refreshes only the changed job's detail and tasks", () => {
        const detail = { ...job("Running"), id: JOB, revision: 2 };
        expect(isAffected(["job", JOB], detail, event())).toBe(true);
        expect(
            isAffected(["job", JOB], { ...detail, revision: 3 }, event()),
        ).toBe(false);
        expect(isAffected(["job", OTHER_JOB], undefined, event())).toBe(false);
        expect(isAffected(["jobs", JOB, "tasks"], undefined, event())).toBe(
            true,
        );
        expect(
            isAffected(["jobs", OTHER_JOB, "tasks"], undefined, event()),
        ).toBe(false);
    });

    it("refreshes a table's reads once one of its jobs ends", () => {
        const ended = event({ state: "Succeeded" });
        expect(
            isAffected(["table", TABLE, "maintenance"], undefined, ended),
        ).toBe(true);
        expect(isAffected(["table", TABLE], undefined, event())).toBe(false);
        expect(isAffected(["table", OTHER_TABLE], undefined, ended)).toBe(
            false,
        );
    });

    it("never touches reads that do not show jobs", () => {
        expect(isAffected(["catalogs", "list", {}], undefined, event())).toBe(
            false,
        );
        expect(isAffected(["session"], undefined, event())).toBe(false);
        expect(isLive(["session"])).toBe(false);
        expect(isLive(["jobs", "list", {}])).toBe(true);
    });
});

describe("LiveUpdates", () => {
    let client: QueryClient;
    let live: LiveUpdates;
    let hidden: boolean;

    function invalidated(key: readonly unknown[]): boolean {
        return client.getQueryState(key)?.isInvalidated ?? false;
    }

    function setHidden(value: boolean) {
        hidden = value;
        document.dispatchEvent(new Event("visibilitychange"));
    }

    beforeEach(() => {
        vi.useFakeTimers();
        hidden = false;
        vi.spyOn(document, "hidden", "get").mockImplementation(() => hidden);
        client = new QueryClient();
        client.setQueryData(["jobs", "list", {}], listed(2));
        client.setQueryData(["jobs", "facets", {}], { state: [] });
        client.setQueryData(["catalogs", "list", {}], { pages: [] });
        client.setQueryData(["table", OTHER_TABLE, "maintenance"], {});
        live = new LiveUpdates(client);
    });

    afterEach(() => {
        live.stop();
        vi.useRealTimers();
    });

    it("reloads job reads once the first stream is ready", () => {
        live.start();
        const source = FakeEventSource.live();
        expect(source.url).toBe("/api/v1/events");

        source.send("ready", {}, "40");
        vi.advanceTimersByTime(LIVE_BATCH_MS);

        expect(invalidated(["jobs", "list", {}])).toBe(true);
        expect(invalidated(["table", OTHER_TABLE, "maintenance"])).toBe(true);
        expect(invalidated(["catalogs", "list", {}])).toBe(false);
    });

    it("batches events and refreshes only the reads they affect", () => {
        live.start();
        const source = FakeEventSource.live();
        source.send("ready", {}, "40");
        vi.advanceTimersByTime(LIVE_BATCH_MS);
        client.setQueryData(["jobs", "list", {}], listed(2));
        client.setQueryData(["jobs", "facets", {}], { state: [] });
        client.setQueryData(["table", OTHER_TABLE, "maintenance"], {});

        source.send("job", event({ revision: 2 }), "41");
        expect(invalidated(["jobs", "facets", {}])).toBe(false);
        vi.advanceTimersByTime(LIVE_BATCH_MS);

        // The list already shows revision 2; the counts always move.
        expect(invalidated(["jobs", "list", {}])).toBe(false);
        expect(invalidated(["jobs", "facets", {}])).toBe(true);
        expect(invalidated(["table", OTHER_TABLE, "maintenance"])).toBe(false);

        source.send("job", event({ revision: 3 }), "42");
        vi.advanceTimersByTime(LIVE_BATCH_MS);
        expect(invalidated(["jobs", "list", {}])).toBe(true);
    });

    it("waits for a running fetch before refreshing its read", async () => {
        live.start();
        const source = FakeEventSource.live();
        source.send("ready", {}, "40");
        let finish: (value: unknown) => void = () => undefined;
        const running = client.query({
            queryKey: ["overview"],
            queryFn: () =>
                new Promise((resolve) => {
                    finish = resolve;
                }),
        });
        vi.advanceTimersByTime(LIVE_BATCH_MS);
        source.send("job", event(), "41");
        vi.advanceTimersByTime(LIVE_BATCH_MS);
        expect(invalidated(["overview"])).toBe(false);

        finish({});
        await running;
        vi.advanceTimersByTime(LIVE_BATCH_MS);
        expect(invalidated(["overview"])).toBe(true);
    });

    it("closes while hidden and resumes after the last ID without a reload", () => {
        live.start();
        const first = FakeEventSource.live();
        first.send("ready", {}, "40");
        first.send("job", event({ job_id: OTHER_JOB }), "41");
        vi.advanceTimersByTime(LIVE_BATCH_MS);

        setHidden(true);
        expect(first.readyState).toBe(FakeEventSource.CLOSED);

        setHidden(false);
        const second = FakeEventSource.live();
        expect(second).not.toBe(first);
        expect(second.url).toBe("/api/v1/events?after=41");

        client.setQueryData(["jobs", "list", {}], listed(9));
        second.send("ready", {}, "41");
        vi.advanceTimersByTime(LIVE_BATCH_MS);
        expect(invalidated(["jobs", "list", {}])).toBe(false);
    });

    it("reloads when the stream resets", () => {
        live.start();
        const source = FakeEventSource.live();
        source.send("ready", {}, "40");
        vi.advanceTimersByTime(LIVE_BATCH_MS);
        client.setQueryData(["jobs", "list", {}], listed(9));

        source.send("reset", {}, "20040");
        vi.advanceTimersByTime(LIVE_BATCH_MS);

        expect(invalidated(["jobs", "list", {}])).toBe(true);
    });

    it("polls and retries when the stream is refused", () => {
        live.start();
        const refused = FakeEventSource.live();
        refused.fail();
        vi.advanceTimersByTime(LIVE_FALLBACK_MS - 1);
        expect(FakeEventSource.live()).toBe(refused);
        expect(invalidated(["jobs", "list", {}])).toBe(false);

        vi.advanceTimersByTime(1 + LIVE_BATCH_MS);
        expect(invalidated(["jobs", "list", {}])).toBe(true);
        expect(FakeEventSource.live()).not.toBe(refused);
    });

    it("lets the browser retry a dropped stream", () => {
        live.start();
        const source = FakeEventSource.live();
        source.drop();
        vi.advanceTimersByTime(LIVE_FALLBACK_MS);
        expect(FakeEventSource.live()).toBe(source);
        expect(source.readyState).toBe(FakeEventSource.CONNECTING);
    });

    it("opens nothing while hidden and stops for good", () => {
        hidden = true;
        live.start();
        expect(() => FakeEventSource.live()).toThrow();

        live.stop();
        setHidden(false);
        expect(() => FakeEventSource.live()).toThrow();
    });
});
