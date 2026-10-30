import { validateJobSearch, validateJobsSearch } from "./search";

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

it("keeps only the job detail's known tabs", () => {
    expect(validateJobSearch({ tab: "statistics" })).toEqual({
        tab: "statistics",
    });
    expect(validateJobSearch({ tab: "tasks" })).toEqual({ tab: "tasks" });
    // The retired name and the default leave the address bare.
    expect(validateJobSearch({ tab: "overview" })).toEqual({});
    expect(validateJobSearch({ tab: 3 })).toEqual({});
});
