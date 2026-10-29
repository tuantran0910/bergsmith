import {
    firstLoadBytes,
    firstLoadFailure,
    lazyChunkFailure,
    lazyChunks,
    stylesheetFailure,
} from "./bundleBudget";

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

describe("firstLoadBytes", () => {
    it("sums the entry and its transitive static imports", () => {
        expect(
            firstLoadBytes([
                chunk("entry.js", 100, {
                    isEntry: true,
                    imports: ["shared.js"],
                }),
                chunk("shared.js", 80, { imports: ["vendor.js"] }),
                chunk("vendor.js", 40),
            ]),
        ).toBe(220);
    });

    it("leaves lazy chunks out of the first load", () => {
        expect(
            firstLoadBytes([
                chunk("entry.js", 100, { isEntry: true }),
                chunk("lazy-page.js", 500),
            ]),
        ).toBe(100);
    });

    it("counts nothing without an entry chunk", () => {
        expect(firstLoadBytes([chunk("orphan.js", 500)])).toBe(0);
    });
});

describe("lazyChunks", () => {
    it("reports chunks outside the entry closure, largest first", () => {
        const report = lazyChunks([
            chunk("entry.js", 100, { isEntry: true, imports: ["shared.js"] }),
            chunk("shared.js", 80),
            chunk("small-page.js", 20),
            chunk("large-page.js", 300),
        ]);
        expect(report.total).toBe(320);
        expect(report.chunks.map((item) => item.fileName)).toEqual([
            "large-page.js",
            "small-page.js",
        ]);
    });

    it("is empty when everything is entry-reachable", () => {
        expect(
            lazyChunks([
                chunk("entry.js", 100, {
                    isEntry: true,
                    imports: ["shared.js"],
                }),
                chunk("shared.js", 80),
            ]),
        ).toEqual({ total: 0, chunks: [] });
    });
});

describe("stylesheetFailure", () => {
    it("accepts stylesheets within the budget", () => {
        expect(stylesheetFailure(11_000, 11_000)).toBeNull();
    });

    it("reports stylesheets over the budget", () => {
        expect(stylesheetFailure(11_001, 11_000)).toBe(
            "Stylesheets are 11.0 kB gzipped, over the 11.0 kB budget",
        );
    });
});

describe("lazyChunkFailure", () => {
    const chunks = [
        chunk("entry.js", 900, { isEntry: true, imports: ["shared.js"] }),
        chunk("shared.js", 900),
        chunk("small.js", 100),
        chunk("big.js", 201),
        chunk("bigger.js", 300),
    ];

    it("ignores first-load chunks and lazy chunks within the budget", () => {
        expect(lazyChunkFailure(chunks, 300)).toBeNull();
    });

    it("lists every lazy chunk over the budget, largest first", () => {
        expect(lazyChunkFailure(chunks, 200)).toBe(
            "Lazy chunks over the 0.2 kB per-chunk budget: bigger.js 0.3 kB, big.js 0.2 kB",
        );
    });
});
