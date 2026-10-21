// Unit tests for the session generation guard, isolated from the full UI so
// the stalled-request race does not depend on React Query's own timing.

import { QueryClient } from "@tanstack/react-query";

import type { SessionInfo } from "./api/auth";
import * as auth from "./api/auth";
import {
    ObsoleteSession,
    SESSION_KEY,
    clearProtected,
    forSession,
    readSession,
    setSession,
} from "./queryClient";

function session(overrides: Partial<SessionInfo> = {}): SessionInfo {
    return {
        organization_id: "org-1",
        principal_id: "alice",
        display_name: "Alice",
        expires_at: "2026-10-22T00:00:00Z",
        grants: [],
        ...overrides,
    };
}

describe("readSession", () => {
    it("ignores a stalled read that resolves after a newer sign-in", async () => {
        const client = new QueryClient();
        setSession(client, session());
        client.setQueryData(["jobs"], ["bobs-cache-must-survive"]);

        // Alice's background refresh stalls.
        let resolveAlice: (value: SessionInfo | null) => void = () => undefined;
        vi.spyOn(auth, "fetchSession").mockReturnValueOnce(
            new Promise((resolve) => {
                resolveAlice = resolve;
            }),
        );
        const stalled = readSession(client);

        // Bob signs in elsewhere; this tab's own re-check lands first and ends
        // Alice's generation.
        const bob = session({ principal_id: "bob", display_name: "Bob" });
        vi.spyOn(auth, "fetchSession").mockResolvedValueOnce(bob);
        await readSession(client);
        setSession(client, bob);
        client.setQueryData(["jobs"], ["bobs-jobs"]);

        // Alice's stale read finally answers. It must not touch Bob's cache.
        resolveAlice(session());
        await expect(stalled).rejects.toBeInstanceOf(ObsoleteSession);

        expect(client.getQueryData(SESSION_KEY)).toEqual(bob);
        expect(client.getQueryData(["jobs"])).toEqual(["bobs-jobs"]);
    });

    it("clears protected caches when the identity differs and the generation still matches", async () => {
        const client = new QueryClient();
        setSession(client, session());
        client.setQueryData(["jobs"], ["alices-jobs"]);

        const bob = session({ principal_id: "bob", display_name: "Bob" });
        vi.spyOn(auth, "fetchSession").mockResolvedValueOnce(bob);
        const next = await readSession(client);

        expect(next).toEqual(bob);
        expect(client.getQueryData(["jobs"])).toBeUndefined();
    });
});

describe("clearProtected", () => {
    it("advances the generation so a request already in flight becomes obsolete", async () => {
        const client = new QueryClient();
        let resolveRequest: (value: string) => void = () => undefined;
        const pending = forSession(
            client,
            () =>
                new Promise<string>((resolve) => {
                    resolveRequest = resolve;
                }),
        );

        clearProtected(client);
        resolveRequest("too late");

        await expect(pending).rejects.toBeInstanceOf(ObsoleteSession);
    });
});
