import { budgetFailure } from "./bundleBudget";

describe("budgetFailure", () => {
    it("passes at or under the budget", () => {
        expect(
            budgetFailure(
                [
                    { fileName: "a.js", gzipBytes: 600 },
                    { fileName: "b.js", gzipBytes: 400 },
                ],
                1000,
            ),
        ).toBeNull();
        expect(budgetFailure([], 0)).toBeNull();
    });

    it("names the total, the budget, and the chunks largest first", () => {
        expect(
            budgetFailure(
                [
                    { fileName: "small.js", gzipBytes: 1_000 },
                    { fileName: "large.js", gzipBytes: 175_500 },
                ],
                175_000,
            ),
        ).toBe(
            "JavaScript is 176.5 kB gzipped, over the 175.0 kB budget: large.js 175.5 kB, small.js 1.0 kB",
        );
    });
});
