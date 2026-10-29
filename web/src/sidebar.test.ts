import { act, renderHook } from "@testing-library/react";

import {
    SIDEBAR_STORAGE_KEY,
    applySidebarPreference,
    isSidebarShortcut,
    readSidebarPreference,
    saveSidebarPreference,
    useSidebarPreference,
} from "./sidebar";

afterEach(() => {
    window.localStorage.clear();
    document.documentElement.removeAttribute("data-sidebar");
    vi.restoreAllMocks();
});

describe("sidebar preference", () => {
    it("defaults to expanded when nothing is stored", () => {
        expect(readSidebarPreference()).toBe("expanded");
    });

    it("reads a stored collapsed choice and ignores anything else", () => {
        window.localStorage.setItem(SIDEBAR_STORAGE_KEY, "collapsed");
        expect(readSidebarPreference()).toBe("collapsed");
        window.localStorage.setItem(SIDEBAR_STORAGE_KEY, "hidden");
        expect(readSidebarPreference()).toBe("expanded");
    });

    it("falls back to expanded when storage refuses access", () => {
        vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
            throw new DOMException("denied", "SecurityError");
        });
        expect(readSidebarPreference()).toBe("expanded");
    });

    it("stores collapsed and forgets it for expanded", () => {
        saveSidebarPreference("collapsed");
        expect(window.localStorage.getItem(SIDEBAR_STORAGE_KEY)).toBe(
            "collapsed",
        );
        saveSidebarPreference("expanded");
        expect(window.localStorage.getItem(SIDEBAR_STORAGE_KEY)).toBeNull();
    });

    it("keeps working when storage refuses writes", () => {
        vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
            throw new DOMException("full", "QuotaExceededError");
        });
        expect(() => {
            saveSidebarPreference("collapsed");
        }).not.toThrow();
    });

    it("marks the root only while collapsed", () => {
        const root = document.documentElement;
        applySidebarPreference("collapsed");
        expect(root).toHaveAttribute("data-sidebar", "collapsed");
        applySidebarPreference("expanded");
        expect(root).not.toHaveAttribute("data-sidebar");
    });

    it("flips, applies, and stores the choice through the hook", () => {
        const { result } = renderHook(() => useSidebarPreference());
        expect(result.current[0]).toBe("expanded");

        act(() => {
            result.current[1]();
        });
        expect(result.current[0]).toBe("collapsed");
        expect(document.documentElement).toHaveAttribute(
            "data-sidebar",
            "collapsed",
        );

        act(() => {
            result.current[1]();
        });
        expect(result.current[0]).toBe("expanded");
        expect(window.localStorage.getItem(SIDEBAR_STORAGE_KEY)).toBeNull();
    });
});

describe("sidebar shortcut", () => {
    const key = (init: KeyboardEventInit) => new KeyboardEvent("keydown", init);

    it("is Ctrl or Command with a backslash", () => {
        expect(isSidebarShortcut(key({ key: "\\", ctrlKey: true }))).toBe(true);
        expect(isSidebarShortcut(key({ key: "\\", metaKey: true }))).toBe(true);
    });

    it("ignores the bare key and other modifiers", () => {
        expect(isSidebarShortcut(key({ key: "\\" }))).toBe(false);
        expect(
            isSidebarShortcut(key({ key: "\\", ctrlKey: true, altKey: true })),
        ).toBe(false);
        expect(
            isSidebarShortcut(
                key({ key: "\\", metaKey: true, shiftKey: true }),
            ),
        ).toBe(false);
        expect(isSidebarShortcut(key({ key: "b", ctrlKey: true }))).toBe(false);
    });
});
