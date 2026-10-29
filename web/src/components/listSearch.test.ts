import { joinValues, searchText, toggleValue, valueList } from "./listSearch";

const STATES = ["Queued", "Running", "Failed"] as const;

it("cleans search text the way the coordinator validates it", () => {
    expect(searchText("  orders ")).toBe("orders");
    expect(searchText("sales.\u0000orders\u007f")).toBe("sales.orders");
    expect(searchText("   ")).toBeUndefined();
    expect(searchText(7)).toBeUndefined();
    // Characters, not UTF-16 units: an emoji counts once.
    expect(Array.from(searchText("🧊".repeat(200)) ?? "")).toHaveLength(128);
});

it("keeps known list values once, in their canonical order", () => {
    expect(valueList("Failed,Queued,Failed,Nope", STATES)).toEqual([
        "Queued",
        "Failed",
    ]);
    expect(valueList("", STATES)).toEqual([]);
    expect(valueList(["Failed"], STATES)).toEqual([]);
    expect(joinValues([])).toBeUndefined();
    expect(joinValues(["Queued", "Failed"])).toBe("Queued,Failed");
    expect(toggleValue(["Failed"], "Queued", STATES)).toEqual([
        "Queued",
        "Failed",
    ]);
    expect(toggleValue(["Queued", "Failed"], "Queued", STATES)).toEqual([
        "Failed",
    ]);
});
