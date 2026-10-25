import { QueryClientProvider } from "@tanstack/react-query";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";
import { reloadOnMissingChunk } from "./chunkReload";
import { applyDensityPreference, readDensityPreference } from "./density";
import { createQueryClient } from "./queryClient";
import "./styles.css";
import { applyThemePreference, readThemePreference } from "./theme";

applyThemePreference(readThemePreference());
applyDensityPreference(readDensityPreference());
reloadOnMissingChunk(window, window.sessionStorage, () => {
    window.location.reload();
});

const root = document.getElementById("root");
if (root !== null) {
    createRoot(root).render(
        <StrictMode>
            <QueryClientProvider client={createQueryClient()}>
                <App />
            </QueryClientProvider>
        </StrictMode>,
    );
}
