import { sortColumn, validateTablesSearch } from "./search";

it("keeps recognized search parameters", () => {
    expect(
        validateTablesSearch({
            q: "orders",
            catalog: "c",
            namespace: "sales",
            sort: "registered",
            order: "desc",
        }),
    ).toEqual({
        q: "orders",
        catalog: "c",
        namespace: "sales",
        sort: "registered",
        order: "desc",
    });
});

it("drops blank, unknown, and malformed values", () => {
    expect(
        validateTablesSearch({
            q: "  ",
            catalog: "",
            namespace: 7,
            sort: "uuid",
            order: "sideways",
        }),
    ).toEqual({});
});

it("recognizes only the columns the table can sort", () => {
    expect(sortColumn("name")).toBe("name");
    expect(sortColumn("nope")).toBeUndefined();
});
