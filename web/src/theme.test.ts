import { act, renderHook } from "@testing-library/react";

import {
    THEME_STORAGE_KEY,
    applyThemePreference,
    readThemePreference,
    saveThemePreference,
    useThemePreference,
} from "./theme";

afterEach(() => {
    window.localStorage.clear();
    document.documentElement.removeAttribute("data-theme");
});

describe("theme preference", () => {
    it("defaults to the system theme when nothing is stored", () => {
        expect(readThemePreference()).toBe("system");
    });

    it("reads a stored light or dark choice", () => {
        window.localStorage.setItem(THEME_STORAGE_KEY, "dark");
        expect(readThemePreference()).toBe("dark");
        window.localStorage.setItem(THEME_STORAGE_KEY, "light");
        expect(readThemePreference()).toBe("light");
    });

    it("ignores an unknown stored value", () => {
        window.localStorage.setItem(THEME_STORAGE_KEY, "sepia");
        expect(readThemePreference()).toBe("system");
    });

    it("falls back to the system theme when storage refuses access", () => {
        vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
            throw new DOMException("denied", "SecurityError");
        });
        expect(readThemePreference()).toBe("system");
    });

    it("stores a pinned choice and forgets it for the system theme", () => {
        saveThemePreference("dark");
        expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("dark");
        saveThemePreference("system");
        expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBeNull();
    });

    it("keeps working when storage refuses writes", () => {
        vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
            throw new DOMException("full", "QuotaExceededError");
        });
        expect(() => {
            saveThemePreference("light");
        }).not.toThrow();
    });

    it("pins light or dark on the root and removes the pin for system", () => {
        const root = document.documentElement;
        applyThemePreference("dark");
        expect(root).toHaveAttribute("data-theme", "dark");
        applyThemePreference("light");
        expect(root).toHaveAttribute("data-theme", "light");
        applyThemePreference("system");
        expect(root).not.toHaveAttribute("data-theme");
    });

    it("applies and stores a choice made through the hook", () => {
        window.localStorage.setItem(THEME_STORAGE_KEY, "light");
        const { result } = renderHook(() => useThemePreference());
        expect(result.current[0]).toBe("light");

        act(() => {
            result.current[1]("dark");
        });

        expect(result.current[0]).toBe("dark");
        expect(document.documentElement).toHaveAttribute("data-theme", "dark");
        expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("dark");
    });
});
