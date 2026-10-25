import { act, renderHook } from "@testing-library/react";

import {
    DENSITY_STORAGE_KEY,
    applyDensityPreference,
    readDensityPreference,
    saveDensityPreference,
    useDensityPreference,
} from "./density";

afterEach(() => {
    window.localStorage.clear();
    document.documentElement.removeAttribute("data-density");
});

describe("density preference", () => {
    it("defaults to comfortable when nothing is stored", () => {
        expect(readDensityPreference()).toBe("comfortable");
    });

    it("reads a stored compact choice", () => {
        window.localStorage.setItem(DENSITY_STORAGE_KEY, "compact");
        expect(readDensityPreference()).toBe("compact");
    });

    it("ignores an unknown stored value", () => {
        window.localStorage.setItem(DENSITY_STORAGE_KEY, "cozy");
        expect(readDensityPreference()).toBe("comfortable");
    });

    it("falls back to comfortable when storage refuses access", () => {
        vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
            throw new DOMException("denied", "SecurityError");
        });
        expect(readDensityPreference()).toBe("comfortable");
    });

    it("stores compact and forgets it for comfortable", () => {
        saveDensityPreference("compact");
        expect(window.localStorage.getItem(DENSITY_STORAGE_KEY)).toBe(
            "compact",
        );
        saveDensityPreference("comfortable");
        expect(window.localStorage.getItem(DENSITY_STORAGE_KEY)).toBeNull();
    });

    it("keeps working when storage refuses writes", () => {
        vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
            throw new DOMException("full", "QuotaExceededError");
        });
        expect(() => {
            saveDensityPreference("compact");
        }).not.toThrow();
    });

    it("marks the root only for compact density", () => {
        const root = document.documentElement;
        applyDensityPreference("compact");
        expect(root).toHaveAttribute("data-density", "compact");
        applyDensityPreference("comfortable");
        expect(root).not.toHaveAttribute("data-density");
    });

    it("applies and stores a choice made through the hook", () => {
        window.localStorage.setItem(DENSITY_STORAGE_KEY, "compact");
        const { result } = renderHook(() => useDensityPreference());
        expect(result.current[0]).toBe("compact");

        act(() => {
            result.current[1]("comfortable");
        });

        expect(result.current[0]).toBe("comfortable");
        expect(document.documentElement).not.toHaveAttribute("data-density");
        expect(window.localStorage.getItem(DENSITY_STORAGE_KEY)).toBeNull();
    });
});
