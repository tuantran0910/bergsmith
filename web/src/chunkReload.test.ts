import { reloadOnMissingChunk } from "./chunkReload";

function harness() {
    const listeners = new Map<string, (event: Event) => void>();
    const store = new Map<string, string>();
    const reload = vi.fn();
    const cleanup = reloadOnMissingChunk(
        {
            addEventListener: (type, listener) => {
                listeners.set(type, listener);
            },
            removeEventListener: (type) => {
                listeners.delete(type);
            },
        },
        {
            getItem: (key) => store.get(key) ?? null,
            setItem: (key, value) => {
                store.set(key, value);
            },
        },
        reload,
    );
    const fire = () => {
        const event = new Event("vite:preloadError", { cancelable: true });
        listeners.get("vite:preloadError")?.(event);
        return event;
    };
    return { fire, reload, cleanup, listeners };
}

it("reloads once when a page chunk disappears", () => {
    const { fire, reload } = harness();

    expect(fire().defaultPrevented).toBe(true);
    expect(reload).toHaveBeenCalledTimes(1);

    fire();
    expect(reload).toHaveBeenCalledTimes(1);
});

it("keeps the error surface when storage refuses a marker", () => {
    const listeners = new Map<string, (event: Event) => void>();
    const reload = vi.fn();
    reloadOnMissingChunk(
        {
            addEventListener: (type, listener) => {
                listeners.set(type, listener);
            },
            removeEventListener: () => undefined,
        },
        {
            getItem: () => {
                throw new DOMException("denied", "SecurityError");
            },
            setItem: () => undefined,
        },
        reload,
    );

    const event = new Event("vite:preloadError", { cancelable: true });
    listeners.get("vite:preloadError")?.(event);

    expect(event.defaultPrevented).toBe(true);
    expect(reload).not.toHaveBeenCalled();
});

it("unsubscribes on cleanup", () => {
    const { cleanup, listeners } = harness();
    cleanup();
    expect(listeners.has("vite:preloadError")).toBe(false);
});
