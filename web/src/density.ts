// The table density preference. "comfortable" is the default; "compact"
// tightens table rows for operators scanning long lists. The choice is a
// per-browser convenience, stored locally, and applied as a data attribute so
// CSS owns the spacing (ADR 0056).

import { useCallback, useState } from "react";

export type Density = "comfortable" | "compact";

export const DENSITIES: readonly Density[] = ["comfortable", "compact"];

/** The `localStorage` key that keeps the preference. */
export const DENSITY_STORAGE_KEY = "bergsmith.density";

function isDensity(value: unknown): value is Density {
    return DENSITIES.includes(value as Density);
}

/** The stored preference, or "comfortable" when none is stored or readable. */
export function readDensityPreference(): Density {
    try {
        const stored = window.localStorage.getItem(DENSITY_STORAGE_KEY);
        return isDensity(stored) ? stored : "comfortable";
    } catch {
        return "comfortable";
    }
}

/** Stores the preference; a browser that refuses storage keeps it for this page. */
export function saveDensityPreference(density: Density): void {
    try {
        if (density === "comfortable") {
            window.localStorage.removeItem(DENSITY_STORAGE_KEY);
        } else {
            window.localStorage.setItem(DENSITY_STORAGE_KEY, density);
        }
    } catch {
        // Storage is optional; the density still applies to this page.
    }
}

/** Applies the preference to `root`. */
export function applyDensityPreference(
    density: Density,
    root: HTMLElement = document.documentElement,
): void {
    if (density === "comfortable") {
        root.removeAttribute("data-density");
    } else {
        root.setAttribute("data-density", density);
    }
}

/** The current preference and a setter that applies and stores a new one. */
export function useDensityPreference(): [Density, (density: Density) => void] {
    const [density, setDensity] = useState(readDensityPreference);
    const choose = useCallback((next: Density) => {
        applyDensityPreference(next);
        saveDensityPreference(next);
        setDensity(next);
    }, []);
    return [density, choose];
}
