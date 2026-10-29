/// <reference types="vitest/config" />
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Vite loads its configuration natively, which needs the file extension.
import { bundleBudget } from "./tooling/bundleBudget.ts";
import { fontPreload } from "./tooling/fontPreload.ts";

/**
 * Gzipped budgets. JavaScript counts the entry graph only; lazy page chunks
 * are excluded. CSS was 9.9 kB when ADR 0067 set its budget at about +10%.
 */
const BUDGETS = { javascriptBytes: 175_000, stylesheetBytes: 11_000 };

// The development server proxies the coordinator so the session and CSRF
// cookies stay same-origin, exactly as when the coordinator serves the build.
const coordinator =
    process.env.BERGSMITH_COORDINATOR_URL ?? "http://127.0.0.1:8080";

export default defineConfig({
    plugins: [react(), tailwindcss(), bundleBudget(BUDGETS), fontPreload()],
    server: {
        host: "127.0.0.1",
        port: 5173,
        strictPort: true,
        proxy: {
            "/api": { target: coordinator },
            "/auth": { target: coordinator },
        },
    },
    build: {
        sourcemap: false,
    },
    test: {
        environment: "jsdom",
        // `e2e/` holds the Playwright browser gate, which Vitest must not run.
        include: ["src/**/*.test.{ts,tsx}", "tooling/**/*.test.ts"],
        globals: true,
        setupFiles: ["src/test/setup.ts"],
        restoreMocks: true,
        coverage: {
            provider: "v8",
            include: ["src/**/*.{ts,tsx}"],
            exclude: [
                "src/**/*.test.{ts,tsx}",
                "src/test/**",
                "src/main.tsx",
                "src/api/schema.d.ts",
            ],
            thresholds: { lines: 80 },
            reporter: ["text-summary", "html"],
        },
    },
});
