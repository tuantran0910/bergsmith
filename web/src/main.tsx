import { CSPProvider } from "@base-ui/react/csp-provider";
import { QueryClientProvider } from "@tanstack/react-query";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";
import { reloadOnMissingChunk } from "./chunkReload";
import { applyDensityPreference, readDensityPreference } from "./density";
import { createQueryClient } from "./queryClient";
import { applySidebarPreference, readSidebarPreference } from "./sidebar";
import "./styles.css";
import { applyThemePreference, readThemePreference } from "./theme";

applyThemePreference(readThemePreference());
applyDensityPreference(readDensityPreference());
applySidebarPreference(readSidebarPreference());
reloadOnMissingChunk(window, window.sessionStorage, () => {
    window.location.reload();
});

const root = document.getElementById("root");
if (root !== null) {
    createRoot(root).render(
        <StrictMode>
            {/* The CSP forbids inline <style> elements; the one rule Base UI
                would inject lives in styles.css instead (ADR 0067). */}
            <CSPProvider disableStyleElements>
                <QueryClientProvider client={createQueryClient()}>
                    <App />
                </QueryClientProvider>
            </CSPProvider>
        </StrictMode>,
    );
}
