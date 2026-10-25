// Vite emits `vite:preloadError` when a dynamic import fails, most often
// because a deploy replaced the hashed chunk an open tab still references.
// Reload once so the tab fetches the new HTML and chunks; a repeated failure
// keeps the normal error surface instead of looping.

const RELOAD_KEY = "bergsmith:chunk-reload";

interface EventTargetLike {
    addEventListener: (type: string, listener: (event: Event) => void) => void;
    removeEventListener: (
        type: string,
        listener: (event: Event) => void,
    ) => void;
}

interface SessionStorageLike {
    getItem: (key: string) => string | null;
    setItem: (key: string, value: string) => void;
}

/** Installs the once-per-session reload; returns its cleanup. */
export function reloadOnMissingChunk(
    target: EventTargetLike,
    storage: SessionStorageLike,
    reload: () => void,
): () => void {
    const listener = (event: Event) => {
        event.preventDefault();
        try {
            if (storage.getItem(RELOAD_KEY) !== null) {
                return;
            }
            storage.setItem(RELOAD_KEY, "1");
        } catch {
            // Storage can be denied; the session cannot be marked, so the
            // route's error surface is safer than an unbounded reload loop.
            return;
        }
        reload();
    };
    target.addEventListener("vite:preloadError", listener);
    return () => {
        target.removeEventListener("vite:preloadError", listener);
    };
}
