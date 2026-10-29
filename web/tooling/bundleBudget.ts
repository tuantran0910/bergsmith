// A size budget for the JavaScript the browser downloads on first load
// (BS-021.T4, refined by ADR 0056), and for the stylesheet (ADR 0067). It counts the entry chunk and the
// chunks statically imported from it, follows that graph transitively, and
// ignores lazy route chunks: what a user pays before any navigation, in
// gzipped bytes. `vite build` fails when that total grows past the budget,
// so growth is a decision rather than drift.

import { gzipSync } from "node:zlib";

import type { Plugin } from "vite";

/** One emitted JavaScript chunk and the graph edges the budget follows. */
export interface EmittedChunk {
    fileName: string;
    gzipBytes: number;
    isEntry: boolean;
    /** Statically imported chunks; dynamic imports are deliberately absent. */
    imports: readonly string[];
}

/** The first-load bytes: entry chunks and their static import closure. */
export function firstLoadBytes(chunks: readonly EmittedChunk[]): number {
    return measured(chunks).total;
}

/** Lazy chunks (everything outside the entry closure), largest first. */
export interface LazyChunks {
    total: number;
    chunks: readonly EmittedChunk[];
}

/** The JavaScript a navigation loads later, largest chunk first. */
export function lazyChunks(chunks: readonly EmittedChunk[]): LazyChunks {
    const counted = new Set(
        measured(chunks).counted.map((chunk) => chunk.fileName),
    );
    const lazy = chunks
        .filter((chunk) => !counted.has(chunk.fileName))
        .sort((left, right) => right.gzipBytes - left.gzipBytes);
    return {
        total: lazy.reduce((sum, chunk) => sum + chunk.gzipBytes, 0),
        chunks: lazy,
    };
}

/** Entry-reachable chunks and their gzipped total. */
function measured(chunks: readonly EmittedChunk[]): {
    total: number;
    counted: readonly EmittedChunk[];
} {
    const byName = new Map(chunks.map((chunk) => [chunk.fileName, chunk]));
    const firstLoad = new Set<string>();
    const queue = chunks
        .filter((chunk) => chunk.isEntry)
        .map((chunk) => chunk.fileName);
    while (queue.length > 0) {
        const name = queue.shift();
        if (name === undefined || firstLoad.has(name)) {
            continue;
        }
        firstLoad.add(name);
        const chunk = byName.get(name);
        if (chunk !== undefined) {
            queue.push(...chunk.imports);
        }
    }
    const counted = chunks.filter((chunk) => firstLoad.has(chunk.fileName));
    return {
        total: counted.reduce((sum, chunk) => sum + chunk.gzipBytes, 0),
        counted,
    };
}

/** Why the first load failed, largest chunk first, or `null` within budget. */
export function firstLoadFailure(
    chunks: readonly EmittedChunk[],
    budgetBytes: number,
): string | null {
    const { total, counted } = measured(chunks);
    if (total <= budgetBytes) {
        return null;
    }
    const largest = [...counted]
        .sort((a, b) => b.gzipBytes - a.gzipBytes)
        .map((chunk) => `${chunk.fileName} ${kilobytes(chunk.gzipBytes)}`)
        .join(", ");
    return `First-load JavaScript is ${kilobytes(total)} gzipped, over the ${kilobytes(budgetBytes)} budget: ${largest}`;
}

/** Why the stylesheets are too big, or `null` within budget. */
export function stylesheetFailure(
    gzipBytes: number,
    budgetBytes: number,
): string | null {
    return gzipBytes <= budgetBytes
        ? null
        : `Stylesheets are ${kilobytes(gzipBytes)} gzipped, over the ${kilobytes(budgetBytes)} budget`;
}

function kilobytes(bytes: number): string {
    return `${(bytes / 1000).toFixed(1)} kB`;
}

/** Gzipped size limits the production build enforces. */
export interface Budgets {
    /** First-load JavaScript: the entry chunk and its static imports. */
    javascriptBytes: number;
    /** Every emitted stylesheet; the UI ships one, loaded up front. */
    stylesheetBytes: number;
}

/** Fails the production build when first-load JavaScript or CSS is too big. */
export function bundleBudget(budgets: Budgets): Plugin {
    const budgetBytes = budgets.javascriptBytes;
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
                              isEntry: output.isEntry,
                              imports: output.imports,
                          },
                      ]
                    : [],
            );
            const firstLoad = firstLoadBytes(chunks);
            this.info(
                `First-load JavaScript: ${kilobytes(firstLoad)} gzipped (budget ${kilobytes(budgetBytes)})`,
            );
            const lazy = lazyChunks(chunks);
            this.info(
                `Lazy JavaScript: ${kilobytes(lazy.total)} gzipped in ${lazy.chunks.length} chunks; ` +
                    lazy.chunks
                        .slice(0, 5)
                        .map(
                            (chunk) =>
                                `${chunk.fileName} ${kilobytes(chunk.gzipBytes)}`,
                        )
                        .join(", "),
            );
            const stylesheets = Object.values(bundle).reduce(
                (sum, output) =>
                    output.type === "asset" && output.fileName.endsWith(".css")
                        ? sum + gzipSync(output.source).length
                        : sum,
                0,
            );
            this.info(
                `Stylesheets: ${kilobytes(stylesheets)} gzipped (budget ${kilobytes(budgets.stylesheetBytes)})`,
            );
            const failure =
                firstLoadFailure(chunks, budgetBytes) ??
                stylesheetFailure(stylesheets, budgets.stylesheetBytes);
            if (failure !== null) {
                this.error(failure);
            }
        },
    };
}
