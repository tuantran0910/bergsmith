// Which columns a list shows (ADR 0067). The choice is a per-browser
// convenience, like theme and density: it lives in local storage, and a
// blocked or cleared store falls back to every column.

import { useCallback, useState } from "react";

/** Column ids mapped to `false` when hidden; absent columns are shown. */
export type ColumnVisibility = Record<string, boolean>;

function read(key: string): ColumnVisibility {
    try {
        const stored = window.localStorage.getItem(key);
        if (stored === null) {
            return {};
        }
        const parsed: unknown = JSON.parse(stored);
        if (typeof parsed !== "object" || parsed === null) {
            return {};
        }
        return Object.fromEntries(
            Object.entries(parsed).filter(
                (entry): entry is [string, boolean] =>
                    typeof entry[1] === "boolean",
            ),
        );
    } catch {
        return {};
    }
}

/** The list's column visibility under `key`, and a setter that keeps it. */
export function useColumnVisibility(
    key: string,
): [ColumnVisibility, (next: ColumnVisibility) => void] {
    const storageKey = `bergsmith.columns.${key}`;
    const [visibility, setVisibility] = useState(() => read(storageKey));
    const update = useCallback(
        (next: ColumnVisibility) => {
            setVisibility(next);
            try {
                window.localStorage.setItem(storageKey, JSON.stringify(next));
            } catch {
                // The choice still applies until the page is closed.
            }
        },
        [storageKey],
    );
    return [visibility, update];
}
