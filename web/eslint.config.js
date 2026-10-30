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
            "test-results",
            "playwright-report",
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
            "no-eval": "error",
            "no-new-func": "error",
            "@typescript-eslint/restrict-template-expressions": [
                "error",
                { allowNumber: true },
            ],
            // TanStack Router ends a route's loading by throwing `notFound()`
            // or `redirect()`, which are values the router catches, not errors.
            "@typescript-eslint/only-throw-error": [
                "error",
                {
                    allow: [
                        {
                            from: "package",
                            package: "@tanstack/router-core",
                            name: ["NotFoundError", "Redirect"],
                        },
                    ],
                },
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
                // String-to-HTML sinks bypass React's escaping; the CSP's
                // Trusted Types would throw at run time, lint fails first.
                {
                    selector:
                        "AssignmentExpression > MemberExpression.left[property.name=/^(innerHTML|outerHTML)$/]",
                    message:
                        "Render text through React; raw HTML is not allowed.",
                },
                {
                    selector:
                        "CallExpression[callee.property.name=/^(insertAdjacentHTML|write|writeln|createContextualFragment)$/]",
                    message:
                        "Render text through React; raw HTML is not allowed.",
                },
                // The CSP allows no inline styles; style through classes and
                // the tokens in styles.css (ADR 0067).
                {
                    selector: "JSXAttribute[name.name='style']",
                    message:
                        "Use classes and design tokens; inline styles are not allowed.",
                },
                // The type scale has two weights (ADR 0067).
                {
                    selector:
                        "Literal[value=/(^|\\s|:)font-(thin|extralight|light|medium|bold|extrabold|black)(\\s|$)/]",
                    message:
                        "Use font-normal or font-semibold; the type scale has two weights.",
                },
                {
                    selector:
                        "TemplateElement[value.raw=/(^|\\s|:)font-(thin|extralight|light|medium|bold|extrabold|black)(\\s|$)/]",
                    message:
                        "Use font-normal or font-semibold; the type scale has two weights.",
                },
            ],
        },
    },
);
