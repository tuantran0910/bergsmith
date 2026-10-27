import type { JobState } from "../api/client";
import {
    canCancel,
    canRetry,
    cancellationMessage,
    executionStatus,
    isTerminal,
    notice,
    publicationStatus,
} from "./status";

// Every state, once. Adding a state to the contract fails the typecheck in
// `status.ts`; adding it here keeps these tables exhaustive.
const STATES: Record<
    JobState,
    { terminal: boolean; execution: string; publication: [string, string] }
> = {
    // publication: [before send authorization, after it]
    Planning: {
        terminal: false,
        execution: "Planning",
        publication: ["Not started", "Commit authorized"],
    },
    Queued: {
        terminal: false,
        execution: "Queued",
        publication: ["Not started", "Commit authorized"],
    },
    Running: {
        terminal: false,
        execution: "Running",
        publication: ["Not started", "Commit authorized"],
    },
    ReadyToCommit: {
        terminal: false,
        execution: "Outputs ready",
        publication: ["Not started", "Commit authorized"],
    },
    Committing: {
        terminal: false,
        execution: "Outputs ready",
        publication: ["Committing", "Committing"],
    },
    CommitOutcomeUnknown: {
        terminal: false,
        execution: "Outputs ready",
        publication: ["Outcome unknown", "Outcome unknown"],
    },
    Deleting: {
        terminal: false,
        execution: "Deleting unreferenced files",
        publication: ["Not applicable", "Not applicable"],
    },
    Cancelling: {
        terminal: false,
        execution: "Stopping after cancellation",
        publication: ["Not started", "Commit authorized"],
    },
    Succeeded: {
        terminal: true,
        execution: "Outputs ready",
        publication: ["Confirmed", "Confirmed"],
    },
    NoOp: {
        terminal: true,
        execution: "Nothing to rewrite",
        publication: ["Not needed", "Not needed"],
    },
    Cancelled: {
        terminal: true,
        execution: "Stopped: cancelled",
        publication: ["Not published", "Not published"],
    },
    ReplanRequired: {
        terminal: true,
        execution: "Stopped: replan required",
        publication: ["Not published", "Not published"],
    },
    Failed: {
        terminal: true,
        execution: "Failed",
        publication: ["Not published", "Not published"],
    },
};

describe.each(
    Object.entries(STATES) as [JobState, (typeof STATES)[JobState]][],
)("%s", (state, expected) => {
    it("reports terminality and the allowed commands", () => {
        expect(isTerminal(state)).toBe(expected.terminal);
        expect(canCancel({ state })).toBe(!expected.terminal);
        expect(canRetry({ state })).toBe(
            expected.terminal && state !== "Succeeded",
        );
    });

    it("reports execution separately from publication", () => {
        expect(
            executionStatus({
                state,
                operation: "compact",
                publication_started: false,
            }).label,
        ).toBe(expected.execution);
        expect(
            publicationStatus({
                state,
                operation: "compact",
                publication_started: false,
            }).label,
        ).toBe(expected.publication[0]);
        expect(
            publicationStatus({
                state,
                operation: "compact",
                publication_started: true,
            }).label,
        ).toBe(expected.publication[1]);
    });

    it("needs a notice only when the operator must act differently", () => {
        const shown = notice(state);
        if (state === "ReplanRequired" || state === "CommitOutcomeUnknown") {
            expect(shown?.action).toBeTruthy();
        } else {
            expect(shown).toBeNull();
        }
    });
});

it("only Succeeded confirms publication", () => {
    const confirmed = (Object.keys(STATES) as JobState[]).filter(
        (state) =>
            publicationStatus({
                state,
                operation: "compact",
                publication_started: true,
            }).label === "Confirmed",
    );
    expect(confirmed).toEqual(["Succeeded"]);
});

it.each([
    ["cancelling", "ok"],
    ["too_late_publication_started", "warning"],
    ["publication_unresolved", "warning"],
    ["too_late_committed", "warning"],
    ["cancelled", "neutral"],
    ["already_ended", "neutral"],
] as const)("describes the %s cancellation outcome", (outcome, tone) => {
    const message = cancellationMessage(outcome);
    expect(message.tone).toBe(tone);
    expect(message.label).not.toBe("");
});

describe("orphan cleanup", () => {
    it("never claims a catalog publication", () => {
        for (const state of [
            "Planning",
            "ReadyToCommit",
            "Deleting",
            "Succeeded",
        ] as JobState[]) {
            expect(
                publicationStatus({
                    state,
                    operation: "remove_orphan_files",
                    publication_started: false,
                }),
            ).toEqual({ label: "Not applicable", tone: "neutral" });
        }
    });
});
