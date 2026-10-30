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
            catalog: "0190a1b2-0000-7000-8000-000000000001",
            sort: "registered",
            order: "desc",
        }),
    ).toEqual({
        q: "orders",
        catalog: "0190a1b2-0000-7000-8000-000000000001",
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

it("drops a catalog that is not a resource ID, which the API would refuse", () => {
    expect(validateTablesSearch({ catalog: "prod" })).toEqual({});
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
