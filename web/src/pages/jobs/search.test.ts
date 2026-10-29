import { validateJobsSearch } from "./search";

it("keeps known job filters in canonical order", () => {
    expect(
        validateJobsSearch({
            q: "  orders ",
            state: "Failed,Running,Bogus",
            operation: "compact,shred",
            order: "oldest",
        }),
    ).toEqual({
        q: "orders",
        state: "Running,Failed",
        operation: "compact",
        order: "oldest",
    });
});

it("drops empty, unknown, and default values", () => {
    expect(
        validateJobsSearch({
            q: " ",
            state: "Bogus",
            operation: "",
            order: "newest",
            page: 3,
        }),
    ).toEqual({});
});
