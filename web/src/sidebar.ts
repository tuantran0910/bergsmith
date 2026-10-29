// The sidebar preference. "expanded" is the default; "collapsed" narrows the
// rail to its icons so wide tables get the room. Like density, the choice is
// a per-browser convenience, stored locally, and applied as a data attribute
// on the root before the first render so CSS owns the layout (ADR 0067).

import { useCallback, useState } from "react";

export type SidebarState = "expanded" | "collapsed";

/** The `localStorage` key that keeps the preference. */
export const SIDEBAR_STORAGE_KEY = "bergsmith.sidebar";

/** The stored preference, or "expanded" when none is stored or readable. */
export function readSidebarPreference(): SidebarState {
    try {
        return window.localStorage.getItem(SIDEBAR_STORAGE_KEY) === "collapsed"
            ? "collapsed"
            : "expanded";
    } catch {
        return "expanded";
    }
}

/** Stores the preference; a browser that refuses storage keeps it for this page. */
export function saveSidebarPreference(state: SidebarState): void {
    try {
        if (state === "expanded") {
            window.localStorage.removeItem(SIDEBAR_STORAGE_KEY);
        } else {
            window.localStorage.setItem(SIDEBAR_STORAGE_KEY, state);
        }
    } catch {
        // Storage is optional; the sidebar still collapses on this page.
    }
}

/** Applies the preference to `root`. */
export function applySidebarPreference(
    state: SidebarState,
    root: HTMLElement = document.documentElement,
): void {
    if (state === "expanded") {
        root.removeAttribute("data-sidebar");
    } else {
        root.setAttribute("data-sidebar", state);
    }
}

/** The current preference and a function that flips, applies, and stores it. */
export function useSidebarPreference(): [SidebarState, () => void] {
    const [state, setState] = useState(readSidebarPreference);
    const toggle = useCallback(() => {
        const next = state === "expanded" ? "collapsed" : "expanded";
        applySidebarPreference(next);
        saveSidebarPreference(next);
        setState(next);
    }, [state]);
    return [state, toggle];
}

/** Whether `event` is the sidebar shortcut: Ctrl+\ or ⌘+\, without Alt or Shift. */
export function isSidebarShortcut(event: KeyboardEvent): boolean {
    return (
        event.key === "\\" &&
        (event.ctrlKey || event.metaKey) &&
        !event.altKey &&
        !event.shiftKey
    );
}
