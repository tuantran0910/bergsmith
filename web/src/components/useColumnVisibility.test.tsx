import { act, renderHook } from "@testing-library/react";

import { useColumnVisibility } from "./useColumnVisibility";

beforeEach(() => {
    window.localStorage.clear();
});

it("remembers hidden columns per list and ignores malformed entries", () => {
    window.localStorage.setItem(
        "bergsmith.columns.jobs",
        JSON.stringify({ state: false, created: "no" }),
    );
    const { result } = renderHook(() => useColumnVisibility("jobs"));
    expect(result.current[0]).toEqual({ state: false });

    act(() => {
        result.current[1]({ state: true, operation: false });
    });
    expect(result.current[0]).toEqual({ state: true, operation: false });
    expect(
        JSON.parse(window.localStorage.getItem("bergsmith.columns.jobs") ?? ""),
    ).toEqual({ state: true, operation: false });
    expect(window.localStorage.getItem("bergsmith.columns.tables")).toBeNull();
});

it("still applies a choice when storage is unavailable", () => {
    const failing = {
        getItem: () => {
            throw new Error("blocked");
        },
        setItem: () => {
            throw new Error("blocked");
        },
    };
    vi.stubGlobal("localStorage", failing);
    const { result } = renderHook(() => useColumnVisibility("jobs"));
    expect(result.current[0]).toEqual({});
    act(() => {
        result.current[1]({ state: false });
    });
    expect(result.current[0]).toEqual({ state: false });
});
