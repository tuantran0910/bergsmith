import {
    healthFilter,
    namespaceKey,
    sortColumn,
    tab,
    validateTablesSearch,
} from "./search";

it("keeps recognized search parameters", () => {
    expect(
        validateTablesSearch({
            q: "orders",
            catalog: "c",
            sort: "registered",
            order: "desc",
        }),
    ).toEqual({
        q: "orders",
        catalog: "c",
        sort: "registered",
        order: "desc",
    });
});

it("keeps namespace levels separate when a level contains a dot", () => {
    expect(namespaceKey(["a", "b"])).not.toBe(namespaceKey(["a.b"]));
});

it("searches as the coordinator does, and drops the old namespace filter", () => {
    expect(validateTablesSearch({ q: " sales.\u0007orders " })).toEqual({
        q: "sales.orders",
    });
    expect(validateTablesSearch({ q: "x".repeat(200) })).toEqual({
        q: "x".repeat(128),
    });
    expect(
        validateTablesSearch({ namespace: namespaceKey(["sales"]) }),
    ).toEqual({});
});

it("drops blank, unknown, and malformed values", () => {
    expect(
        validateTablesSearch({
            q: "  ",
            catalog: "",
            namespace: "sales",
            sort: "uuid",
            order: "sideways",
        }),
    ).toEqual({});
});

it("recognizes only the columns the table can sort", () => {
    expect(sortColumn("name")).toBe("name");
    expect(sortColumn("health")).toBe("health");
    expect(sortColumn("nope")).toBeUndefined();
});

it("keeps only a known health filter and opens the health tab", () => {
    expect(validateTablesSearch({ health: "critical" })).toEqual({
        health: "critical",
    });
    expect(validateTablesSearch({ health: "broken" })).toEqual({});
    expect(healthFilter("unassessed")).toBe("unassessed");
    expect(healthFilter("healthy")).toBe("healthy");
    expect(healthFilter("Critical")).toBeUndefined();
    expect(tab("health")).toBe("health");
});
