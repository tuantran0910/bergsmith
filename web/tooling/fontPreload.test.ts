import { latinFontFile, preloadTag } from "./fontPreload";

describe("latinFontFile", () => {
    it("finds the hashed Latin variable-weight file", () => {
        expect(
            latinFontFile([
                "assets/index-abc123.js",
                "assets/inter-latin-ext-wght-normal-DEF456.woff2",
                "assets/inter-latin-wght-normal-Ab_9-z.woff2",
                "assets/inter-vietnamese-wght-normal-GHI789.woff2",
            ]),
        ).toBe("assets/inter-latin-wght-normal-Ab_9-z.woff2");
    });

    it("does not mistake another subset for Latin", () => {
        expect(
            latinFontFile([
                "assets/inter-latin-ext-wght-normal-DEF456.woff2",
                "assets/inter-latin-wght-italic-DEF456.woff2",
            ]),
        ).toBeNull();
    });
});

describe("preloadTag", () => {
    it("preloads the font in CORS mode under the configured base", () => {
        expect(
            preloadTag("/", "assets/inter-latin-wght-normal-x.woff2"),
        ).toEqual({
            tag: "link",
            attrs: {
                rel: "preload",
                href: "/assets/inter-latin-wght-normal-x.woff2",
                as: "font",
                type: "font/woff2",
                crossorigin: "",
            },
            injectTo: "head",
        });
    });
});
