import { QueryClientProvider } from "@tanstack/react-query";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";
import { createQueryClient } from "./queryClient";
import "./styles.css";
import { applyThemePreference, readThemePreference } from "./theme";

applyThemePreference(readThemePreference());

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
