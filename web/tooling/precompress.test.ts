import { brotliDecompressSync, gunzipSync } from "node:zlib";

import { MINIMUM_BYTES, compress, isCompressible } from "./precompress";

describe("precompressed build copies", () => {
    it("are written for text files of at least 1 KiB", () => {
        for (const name of [
            "index.html",
            "assets/index-abc.js",
            "assets/index-abc.css",
            "assets/icon.svg",
        ]) {
            expect(isCompressible(name, MINIMUM_BYTES)).toBe(true);
        }
    });

    it("are skipped for small files and compressed formats", () => {
        expect(isCompressible("assets/tiny.js", MINIMUM_BYTES - 1)).toBe(false);
        expect(
            isCompressible("assets/inter-latin-wght-normal-x.woff2", 50_000),
        ).toBe(false);
        expect(isCompressible("assets/logo.png", 50_000)).toBe(false);
        expect(isCompressible("assets/app.js.br", 50_000)).toBe(false);
    });

    it("decode to the original bytes and are smaller", () => {
        const source = Buffer.from(
            "export const label = 'compact density';\n".repeat(200),
        );
        const { br, gz } = compress(source);

        expect(brotliDecompressSync(br).equals(source)).toBe(true);
        expect(gunzipSync(gz).equals(source)).toBe(true);
        expect(br.length).toBeLessThan(gz.length);
        expect(gz.length).toBeLessThan(source.length);
    });
});
