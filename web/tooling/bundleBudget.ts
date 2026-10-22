// A size budget for the JavaScript the browser downloads (BS-021.T4). It
// counts gzipped bytes, what a user pays for, and fails `vite build` when the
// total grows past the budget, so growth is a decision rather than drift.

import { gzipSync } from "node:zlib";

import type { Plugin } from "vite";

/** One emitted file and its gzipped size. */
export interface ChunkSize {
    fileName: string;
    gzipBytes: number;
}

/** Why the budget failed, largest chunk first, or `null` within budget. */
export function budgetFailure(
    chunks: readonly ChunkSize[],
    budgetBytes: number,
): string | null {
    const total = chunks.reduce((sum, chunk) => sum + chunk.gzipBytes, 0);
    if (total <= budgetBytes) {
        return null;
    }
    const largest = [...chunks]
        .sort((a, b) => b.gzipBytes - a.gzipBytes)
        .map((chunk) => `${chunk.fileName} ${kilobytes(chunk.gzipBytes)}`)
        .join(", ");
    return `JavaScript is ${kilobytes(total)} gzipped, over the ${kilobytes(budgetBytes)} budget: ${largest}`;
}

function kilobytes(bytes: number): string {
    return `${(bytes / 1000).toFixed(1)} kB`;
}

/** Fails the production build when gzipped JavaScript exceeds `budgetBytes`. */
export function bundleBudget(budgetBytes: number): Plugin {
    return {
        name: "bergsmith-bundle-budget",
        apply: "build",
        generateBundle(_options, bundle) {
            const chunks = Object.values(bundle).flatMap((output) =>
                output.type === "chunk"
                    ? [
                          {
                              fileName: output.fileName,
                              gzipBytes: gzipSync(output.code).length,
                          },
                      ]
                    : [],
            );
            const failure = budgetFailure(chunks, budgetBytes);
            if (failure !== null) {
                this.error(failure);
            }
        },
    };
}
