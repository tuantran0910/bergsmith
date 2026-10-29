// Preloads the interface font's Latin subset (BS-033.T1, ADR 0067). The
// stylesheet that declares the font is discovered only after the CSS loads,
// so without a preload the first paint waits on a second round trip. Only the
// Latin file is preloaded: the other subsets load on demand through
// `unicode-range`, and preloading them would spend bandwidth most pages never
// use. The build fails when the file is missing, so a renamed subset cannot
// silently drop the preload.

import type { HtmlTagDescriptor, Plugin } from "vite";

/** The emitted Latin variable-weight file, matched by its hashed name. */
const LATIN_FONT = /(^|\/)inter-latin-wght-normal-[\w-]+\.woff2$/;

/** The emitted file to preload, or `null` when the build has none. */
export function latinFontFile(fileNames: readonly string[]): string | null {
    return fileNames.find((name) => LATIN_FONT.test(name)) ?? null;
}

/**
 * The preload tag for one font file. Fonts are fetched in CORS mode even on
 * the same origin, so the tag needs `crossorigin` or the browser downloads the
 * file twice.
 */
export function preloadTag(base: string, fileName: string): HtmlTagDescriptor {
    return {
        tag: "link",
        attrs: {
            rel: "preload",
            href: `${base}${fileName}`,
            as: "font",
            type: "font/woff2",
            crossorigin: "",
        },
        injectTo: "head",
    };
}

/** Adds a `<link rel="preload">` for the Latin font to the built page. */
export function fontPreload(): Plugin {
    let base = "/";
    return {
        name: "bergsmith-font-preload",
        apply: "build",
        configResolved(config) {
            base = config.base;
        },
        transformIndexHtml: {
            order: "post",
            handler(_html, context) {
                const fileNames = Object.keys(context.bundle ?? {});
                const file = latinFontFile(fileNames);
                if (file === null) {
                    throw new Error(
                        "the Latin interface font is missing from the build, so it cannot be preloaded",
                    );
                }
                return [preloadTag(base, file)];
            },
        },
    };
}
