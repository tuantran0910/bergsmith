import { fireEvent, render, screen } from "@testing-library/react";

import { FacetFilter, facetOptions, FilterPills } from "./FacetFilter";

/** The narrow layout, where a facet renders its choices in place. */
function narrow() {
    vi.stubGlobal(
        "matchMedia",
        vi.fn((query: string) => ({
            matches: false,
            media: query,
            addEventListener: () => undefined,
            removeEventListener: () => undefined,
        })),
    );
}

it("keeps a selected value the counts no longer include", () => {
    const label = (value: string) => value.toUpperCase();
    expect(
        facetOptions([{ value: "running", count: 3 }], ["failed"], label),
    ).toEqual([
        { value: "running", label: "RUNNING", count: 3 },
        { value: "failed", label: "FAILED", count: 0 },
    ]);
    // While counts load, a selection shows without a number.
    expect(facetOptions(undefined, ["failed"], label)).toEqual([
        { value: "failed", label: "FAILED", count: undefined },
    ]);
});

it("toggles values of a multi-value facet", () => {
    narrow();
    const onChange = vi.fn();
    render(
        <FacetFilter
            label="State"
            multiple
            options={[
                { value: "Running", label: "Running", count: 2 },
                { value: "Failed", label: "Failed", count: 1 },
            ]}
            selected={["Failed"]}
            onChange={onChange}
        />,
    );
    expect(screen.getByRole("group", { name: "State" })).toBeInTheDocument();
    expect(screen.getByRole("checkbox", { name: "Failed 1" })).toBeChecked();
    fireEvent.click(screen.getByRole("checkbox", { name: "Running 2" }));
    expect(onChange).toHaveBeenLastCalledWith(["Failed", "Running"]);
    // The first choice shows at once, before the page passes it back.
    expect(screen.getByRole("checkbox", { name: "Running 2" })).toBeChecked();
    fireEvent.click(screen.getByRole("checkbox", { name: "Failed 1" }));
    expect(onChange).toHaveBeenLastCalledWith(["Running"]);
});

it("offers one value or all for a single-value facet", () => {
    narrow();
    const onChange = vi.fn();
    render(
        <FacetFilter
            label="Health"
            allLabel="All states"
            options={[{ value: "critical", label: "Critical", count: 4 }]}
            selected={["critical"]}
            onChange={onChange}
        />,
    );
    expect(screen.getByRole("radio", { name: "Critical 4" })).toBeChecked();
    fireEvent.click(screen.getByRole("radio", { name: "All states" }));
    expect(onChange).toHaveBeenLastCalledWith([]);
});

it("names its button starting with the visible label and choice", () => {
    vi.stubGlobal(
        "matchMedia",
        vi.fn((query: string) => ({
            matches: true,
            media: query,
            addEventListener: () => undefined,
            removeEventListener: () => undefined,
        })),
    );
    render(
        <FacetFilter
            label="Health"
            allLabel="All states"
            options={[{ value: "critical", label: "Critical", count: 4 }]}
            selected={["critical"]}
            onChange={vi.fn()}
        />,
    );
    expect(
        screen.getByRole("button", { name: /^Health Critical/ }),
    ).toHaveAccessibleName("Health Critical filter");
});

it("removes one filter from its pill", () => {
    const onRemove = vi.fn();
    render(
        <FilterPills
            pills={[{ key: "s", label: "State", value: "Failed", onRemove }]}
        />,
    );
    fireEvent.click(
        screen.getByRole("button", { name: "Remove filter State: Failed" }),
    );
    expect(onRemove).toHaveBeenCalledTimes(1);
    const { container } = render(<FilterPills pills={[]} />);
    expect(container).toBeEmptyDOMElement();
});
