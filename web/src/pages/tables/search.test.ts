import {
    healthFilter,
    namespaceKey,
    parseNamespaceKey,
    sortColumn,
    tab,
    validateTablesSearch,
} from "./search";

it("keeps recognized search parameters", () => {
    expect(
        validateTablesSearch({
            q: "orders",
            catalog: "c",
            namespace: namespaceKey(["sales"]),
            sort: "registered",
            order: "desc",
        }),
    ).toEqual({
        q: "orders",
        catalog: "c",
        namespace: namespaceKey(["sales"]),
        sort: "registered",
        order: "desc",
    });
});

it("keeps namespace levels separate when a level contains a dot", () => {
    expect(
        validateTablesSearch({ namespace: namespaceKey(["a", "b"]) }),
    ).toEqual({ namespace: namespaceKey(["a", "b"]) });
    expect(parseNamespaceKey(namespaceKey(["a.b"]))).toEqual(["a.b"]);
    expect(namespaceKey(["a", "b"])).not.toBe(namespaceKey(["a.b"]));
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
    expect(validateTablesSearch({ namespace: '["a", 7]' })).toEqual({});
    expect(validateTablesSearch({ namespace: "{}" })).toEqual({});
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
