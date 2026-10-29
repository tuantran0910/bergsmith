import { fireEvent, render, screen } from "@testing-library/react";

import { FilterBar } from "./FilterBar";

it("labels the search field and reports each change", () => {
    const onQueryChange = vi.fn();
    render(
        <FilterBar
            query="lake"
            queryLabel="Search tables"
            placeholder="Search by name"
            onQueryChange={onQueryChange}
        />,
    );

    const search = screen.getByRole("searchbox", { name: "Search tables" });
    expect(search).toHaveValue("lake");
    expect(search).toHaveAttribute("placeholder", "Search by name");

    fireEvent.change(search, { target: { value: "orders" } });
    expect(onQueryChange).toHaveBeenCalledWith("orders");
});

it("offers the caller's filters, a clear action, and a result summary", () => {
    const onClear = vi.fn();
    render(
        <FilterBar
            query=""
            queryLabel="Search tables"
            onQueryChange={() => undefined}
            onClear={onClear}
            result="2 of 8 tables"
        >
            <select aria-label="Catalog">
                <option>lake</option>
            </select>
        </FilterBar>,
    );

    expect(screen.getByRole("combobox", { name: "Catalog" })).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Clear filters" }));
    expect(onClear).toHaveBeenCalledTimes(1);
    expect(screen.getByText("2 of 8 tables")).toHaveAttribute(
        "aria-live",
        "polite",
    );
});

it("keeps the clear action out when the page has none", () => {
    render(
        <FilterBar
            query=""
            queryLabel="Search tables"
            onQueryChange={() => undefined}
        />,
    );
    expect(
        screen.queryByRole("button", { name: "Clear filters" }),
    ).not.toBeInTheDocument();
});

it("moves the filters into a drawer when narrow", () => {
    vi.stubGlobal(
        "matchMedia",
        vi.fn().mockImplementation((query: string) => ({
            matches: false,
            media: query,
            addEventListener: vi.fn(),
            removeEventListener: vi.fn(),
        })),
    );
    render(
        <FilterBar
            query=""
            queryLabel="Search tables"
            onQueryChange={() => undefined}
            result="2 of 8 tables"
        >
            <select aria-label="Health">
                <option>healthy</option>
            </select>
        </FilterBar>,
    );

    expect(
        screen.queryByRole("combobox", { name: "Health" }),
    ).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Filters" }));
    expect(screen.getByRole("dialog", { name: "Filters" })).toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "Health" })).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Show results" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
});

it("keeps the filters inline above the small breakpoint", () => {
    vi.stubGlobal(
        "matchMedia",
        vi.fn().mockImplementation((query: string) => ({
            matches: true,
            media: query,
            addEventListener: vi.fn(),
            removeEventListener: vi.fn(),
        })),
    );
    render(
        <FilterBar
            query=""
            queryLabel="Search tables"
            onQueryChange={() => undefined}
        >
            <select aria-label="Health">
                <option>healthy</option>
            </select>
        </FilterBar>,
    );

    expect(screen.getByRole("combobox", { name: "Health" })).toBeVisible();
    expect(
        screen.queryByRole("button", { name: "Filters" }),
    ).not.toBeInTheDocument();
});
