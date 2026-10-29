import { act, fireEvent, render, screen, within } from "@testing-library/react";

import { FilterBar } from "./FilterBar";

/** A matchMedia stub whose min-width answer follows a settable viewport. */
function stubMedia(initialWidth = 320) {
    const listeners = new Set<() => void>();
    let width = initialWidth;
    vi.stubGlobal(
        "matchMedia",
        vi.fn((query: string) => ({
            matches: query === "(min-width: 40rem)" ? width >= 640 : false,
            media: query,
            addEventListener: (_event: string, listener: () => void) => {
                listeners.add(listener);
            },
            removeEventListener: (_event: string, listener: () => void) => {
                listeners.delete(listener);
            },
        })),
    );
    return {
        setWidth(next: number) {
            width = next;
            for (const listener of listeners) {
                listener();
            }
        },
    };
}

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
    stubMedia(320);
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
    const drawer = screen.getByRole("dialog", { name: "Filters" });
    expect(drawer).toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "Health" })).toBeVisible();
    expect(within(drawer).getByText("2 of 8 tables")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Show results" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByText("2 of 8 tables")).toBeInTheDocument();
});

it("closes the drawer and returns focus when the layout widens", async () => {
    const media = stubMedia(320);
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
    fireEvent.click(screen.getByRole("button", { name: "Filters" }));
    expect(screen.getByRole("dialog", { name: "Filters" })).toBeInTheDocument();

    await act(async () => {
        media.setWidth(800);
        await Promise.resolve();
    });
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(
        screen.getByRole("searchbox", { name: "Search tables" }),
    ).toHaveFocus();

    await act(async () => {
        media.setWidth(320);
        await Promise.resolve();
    });
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
});

it("keeps the filters inline above the small breakpoint", () => {
    stubMedia(800);
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

it("reports the search once typing pauses, capped at the API's length", () => {
    vi.useFakeTimers();
    try {
        const onQueryChange = vi.fn();
        const { rerender } = render(
            <FilterBar
                query=""
                queryLabel="Search jobs"
                onQueryChange={onQueryChange}
                delay={150}
            />,
        );
        const field = screen.getByRole("searchbox", { name: "Search jobs" });
        expect(field).toHaveAttribute("maxLength", "128");

        fireEvent.change(field, { target: { value: "o" } });
        fireEvent.change(field, { target: { value: "or" } });
        act(() => {
            vi.advanceTimersByTime(149);
        });
        expect(onQueryChange).not.toHaveBeenCalled();
        fireEvent.change(field, { target: { value: "orders" } });
        act(() => {
            vi.advanceTimersByTime(150);
        });
        expect(onQueryChange).toHaveBeenCalledTimes(1);
        expect(onQueryChange).toHaveBeenCalledWith("orders");

        // The page echoing the report leaves the field alone; the page
        // clearing it empties the field.
        rerender(
            <FilterBar
                query="orders"
                queryLabel="Search jobs"
                onQueryChange={onQueryChange}
                delay={150}
            />,
        );
        expect(field).toHaveValue("orders");
        rerender(
            <FilterBar
                query=""
                queryLabel="Search jobs"
                onQueryChange={onQueryChange}
                delay={150}
            />,
        );
        expect(field).toHaveValue("");
        act(() => {
            vi.advanceTimersByTime(300);
        });
        expect(onQueryChange).toHaveBeenCalledTimes(1);
    } finally {
        vi.useRealTimers();
    }
});
