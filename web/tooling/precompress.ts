// Writes Brotli and gzip copies of the built text files (BS-033.T3, ADR
// 0067). The coordinator serves `name.br` or `name.gz` to a client that
// accepts the coding and never compresses at run time; the development nginx
// serves the gzip copies. Fonts and images are already compressed, and a
// file under 1 KiB gains less than the bytes of the extra header.

import { readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { brotliCompressSync, constants, gzipSync } from "node:zlib";

import type { Plugin } from "vite";

const COMPRESSIBLE = /\.(?:html|m?js|css|json|map|svg|txt)$/;

/** Files smaller than this are left as built. */
export const MINIMUM_BYTES = 1024;

/** Whether the build writes compressed copies of `fileName`. */
export function isCompressible(fileName: string, size: number): boolean {
    return COMPRESSIBLE.test(fileName) && size >= MINIMUM_BYTES;
}

/** The Brotli (best quality) and gzip (level 9) copies of `source`. */
export function compress(source: Buffer): { br: Buffer; gz: Buffer } {
    return {
        br: brotliCompressSync(source, {
            params: {
                [constants.BROTLI_PARAM_QUALITY]: constants.BROTLI_MAX_QUALITY,
                [constants.BROTLI_PARAM_MODE]: constants.BROTLI_MODE_TEXT,
                [constants.BROTLI_PARAM_SIZE_HINT]: source.length,
            },
        }),
        gz: gzipSync(source, { level: 9 }),
    };
}

/** Writes `.br` and `.gz` beside each compressible file of the build. */
export function precompress(): Plugin {
    return {
        name: "bergsmith-precompress",
        apply: "build",
        async writeBundle(options, bundle) {
            const directory = options.dir;
            if (directory === undefined) {
                return;
            }
            await Promise.all(
                Object.keys(bundle).map(async (fileName) => {
                    const path = join(directory, fileName);
                    const source = await readFile(path);
                    if (!isCompressible(fileName, source.length)) {
                        return;
                    }
                    const { br, gz } = compress(source);
                    await Promise.all([
                        writeFile(`${path}.br`, br),
                        writeFile(`${path}.gz`, gz),
                    ]);
                }),
            );
        },
    };
}
