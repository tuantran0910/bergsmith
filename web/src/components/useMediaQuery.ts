// A CSS media query as React state (BS-032.T4). Components that must render a
// control in exactly one place (an inline filter bar or a mobile drawer) use
// this instead of rendering two copies and hiding one.

import { useCallback, useSyncExternalStore } from "react";

/** Whether `query` currently matches; true where `matchMedia` is unavailable. */
export function useMediaQuery(query: string): boolean {
    const subscribe = useCallback(
        (onChange: () => void) => {
            if (typeof window.matchMedia !== "function") {
                return () => undefined;
            }
            const list = window.matchMedia(query);
            list.addEventListener("change", onChange);
            return () => {
                list.removeEventListener("change", onChange);
            };
        },
        [query],
    );
    const read = () =>
        typeof window.matchMedia === "function"
            ? window.matchMedia(query).matches
            : true;
    return useSyncExternalStore(subscribe, read, () => true);
}
