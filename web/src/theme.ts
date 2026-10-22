// The color theme. "system" follows the operating system through CSS
// `light-dark()` and needs no script; "light" and "dark" pin the theme with a
// `data-theme` attribute on the root element. The choice is a per-browser
// convenience, so storage that is missing or refuses access falls back to
// "system".

import { useCallback, useState } from "react";

export type ThemePreference = "system" | "light" | "dark";

export const THEME_PREFERENCES: readonly ThemePreference[] = [
    "system",
    "light",
    "dark",
];

/** The `localStorage` key that keeps the preference. */
export const THEME_STORAGE_KEY = "bergsmith.theme";

function isThemePreference(value: unknown): value is ThemePreference {
    return THEME_PREFERENCES.includes(value as ThemePreference);
}

/** The stored preference, or "system" when none is stored or readable. */
export function readThemePreference(): ThemePreference {
    try {
        const stored = window.localStorage.getItem(THEME_STORAGE_KEY);
        return isThemePreference(stored) ? stored : "system";
    } catch {
        return "system";
    }
}

/** Stores the preference; a browser that refuses storage keeps it for this page only. */
export function saveThemePreference(preference: ThemePreference): void {
    try {
        if (preference === "system") {
            window.localStorage.removeItem(THEME_STORAGE_KEY);
        } else {
            window.localStorage.setItem(THEME_STORAGE_KEY, preference);
        }
    } catch {
        // Storage is optional; the theme still applies to this page.
    }
}

/** Applies the preference to `root`. */
export function applyThemePreference(
    preference: ThemePreference,
    root: HTMLElement = document.documentElement,
): void {
    if (preference === "system") {
        root.removeAttribute("data-theme");
    } else {
        root.setAttribute("data-theme", preference);
    }
}

/** The current preference and a setter that applies and stores a new one. */
export function useThemePreference(): [
    ThemePreference,
    (preference: ThemePreference) => void,
] {
    const [preference, setPreference] = useState(readThemePreference);
    const choose = useCallback((next: ThemePreference) => {
        applyThemePreference(next);
        saveThemePreference(next);
        setPreference(next);
    }, []);
    return [preference, choose];
}
