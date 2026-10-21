import js from "@eslint/js";
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
    {
        rules: {
            "@typescript-eslint/restrict-template-expressions": [
                "error",
                { allowNumber: true },
            ],
        },
    },
);
