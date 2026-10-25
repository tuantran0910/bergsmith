import { firstLoadFailure } from "./bundleBudget";

function chunk(
    fileName: string,
    gzipBytes: number,
    options: { isEntry?: boolean; imports?: string[] } = {},
) {
    return {
        fileName,
        gzipBytes,
        isEntry: options.isEntry ?? false,
        imports: options.imports ?? [],
    };
}

describe("firstLoadFailure", () => {
    it("accepts a total within the budget", () => {
        expect(
            firstLoadFailure(
                [
                    chunk("entry.js", 100, {
                        isEntry: true,
                        imports: ["shared.js"],
                    }),
                    chunk("shared.js", 50),
                ],
                150,
            ),
        ).toBeNull();
    });

    it("follows static imports transitively and names the largest", () => {
        const failure = firstLoadFailure(
            [
                chunk("entry.js", 100_000, {
                    isEntry: true,
                    imports: ["shared.js"],
                }),
                chunk("shared.js", 80_000, { imports: ["vendor.js"] }),
                chunk("vendor.js", 40_000),
            ],
            200_000,
        );
        expect(failure).toContain("First-load JavaScript is 220.0 kB");
        expect(failure).toContain("entry.js 100.0 kB");
        expect(failure).toContain("shared.js 80.0 kB");
        expect(failure).toContain("vendor.js 40.0 kB");
    });

    it("ignores lazy chunks that the entry does not import", () => {
        expect(
            firstLoadFailure(
                [
                    chunk("entry.js", 100, { isEntry: true }),
                    chunk("lazy-page.js", 500),
                ],
                150,
            ),
        ).toBeNull();
    });

    it("treats an empty bundle as free", () => {
        expect(firstLoadFailure([], 0)).toBeNull();
    });
});
