import js from "@eslint/js";
import jsxA11y from "eslint-plugin-jsx-a11y-x";
import reactHooks from "eslint-plugin-react-hooks";
import globals from "globals";
import tseslint from "typescript-eslint";

export default tseslint.config(
    {
        ignores: [
            "dist",
            "coverage",
            "src/api/schema.d.ts",
            "eslint.config.js",
        ],
    },
    js.configs.recommended,
    ...tseslint.configs.strictTypeChecked,
    {
        languageOptions: {
            globals: { ...globals.browser },
            parserOptions: {
                projectService: true,
                tsconfigRootDir: import.meta.dirname,
            },
        },
    },
    reactHooks.configs.flat.recommended,
    jsxA11y.configs.strict,
    {
        rules: {
            "@typescript-eslint/restrict-template-expressions": [
                "error",
                { allowNumber: true },
            ],
            // Raw HTML from any source bypasses React's escaping (ADR 0046).
            "no-restricted-syntax": [
                "error",
                {
                    selector:
                        "JSXAttribute[name.name='dangerouslySetInnerHTML']",
                    message:
                        "Render text through React; raw HTML is not allowed.",
                },
            ],
        },
    },
);
