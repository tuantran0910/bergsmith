/// <reference types="vitest/config" />
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// The development server proxies the coordinator so the session and CSRF
// cookies stay same-origin, exactly as when the coordinator serves the build.
const coordinator =
    process.env.BERGSMITH_COORDINATOR_URL ?? "http://127.0.0.1:8080";

export default defineConfig({
    plugins: [react()],
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
