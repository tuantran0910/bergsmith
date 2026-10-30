import { fireEvent, render, screen, within } from "@testing-library/react";

import { DataGrid, nextRow, VIRTUALIZE_AFTER } from "./DataGrid";
import type { DataTableColumn } from "./DataTable";

interface Item {
    id: string;
    name: string;
    size: number;
}

const columns: DataTableColumn<Item>[] = [
    {
        id: "name",
        header: "Name",
        sortValue: (item) => item.name,
        cell: (item) => <a href={`#${item.id}`}>{item.name}</a>,
    },
    {
        id: "size",
        header: "Size",
        align: "end",
        sortValue: (item) => item.size,
        cell: (item) => item.size,
    },
    { id: "created", header: "Created", serverSort: true, cell: () => "—" },
];

function items(count: number): Item[] {
    return Array.from({ length: count }, (_value, index) => ({
        id: `item-${index}`,
        name: `item ${String(index).padStart(4, "0")}`,
        size: count - index,
    }));
}

it("moves between rows with arrows, Home, and End", () => {
    expect(nextRow("ArrowDown", 0, 3)).toBe(1);
    expect(nextRow("ArrowDown", 2, 3)).toBe(2);
    expect(nextRow("ArrowUp", 0, 3)).toBe(0);
    expect(nextRow("Home", 2, 3)).toBe(0);
    expect(nextRow("End", 0, 3)).toBe(2);
    expect(nextRow("Enter", 0, 3)).toBeUndefined();

    render(
        <DataGrid
            rows={items(3)}
            columns={columns}
            getRowKey={(item) => item.id}
            caption="Items"
        />,
    );
    const first = screen.getByRole("link", { name: "item 0000" });
    first.focus();
    fireEvent.keyDown(first, { key: "ArrowDown" });
    expect(screen.getByRole("link", { name: "item 0001" })).toHaveFocus();
    fireEvent.keyDown(document.activeElement as HTMLElement, { key: "End" });
    expect(screen.getByRole("link", { name: "item 0002" })).toHaveFocus();
    // A modified key is left to the browser.
    fireEvent.keyDown(document.activeElement as HTMLElement, {
        key: "Home",
        shiftKey: true,
    });
    expect(screen.getByRole("link", { name: "item 0002" })).toHaveFocus();
});

it("sorts browser columns, and leaves server-sorted rows in order", () => {
    const onSortChange = vi.fn();
    const { rerender } = render(
        <DataGrid
            rows={items(3)}
            columns={columns}
            getRowKey={(item) => item.id}
            sort={{ column: "size", direction: "asc" }}
            onSortChange={onSortChange}
            caption="Items"
        />,
    );
    const names = () =>
        screen
            .getAllByRole("link")
            .map((link) => link.textContent)
            .filter((text) => text.startsWith("item"));
    expect(names()).toEqual(["item 0002", "item 0001", "item 0000"]);
    expect(screen.getByRole("columnheader", { name: /Size/ })).toHaveAttribute(
        "aria-sort",
        "ascending",
    );

    rerender(
        <DataGrid
            rows={items(3)}
            columns={columns}
            getRowKey={(item) => item.id}
            sort={{ column: "created", direction: "asc" }}
            onSortChange={onSortChange}
            caption="Items"
        />,
    );
    expect(names()).toEqual(["item 0000", "item 0001", "item 0002"]);
    fireEvent.click(screen.getByRole("button", { name: "Created" }));
    expect(onSortChange).toHaveBeenCalledWith({
        column: "created",
        direction: "desc",
    });
});

it("hides columns the viewer turned off, but never the first", () => {
    render(
        <DataGrid
            rows={items(2)}
            columns={columns}
            getRowKey={(item) => item.id}
            columnVisibility={{ name: false, size: false }}
            caption="Items"
        />,
    );
    const headers = screen
        .getAllByRole("columnheader")
        .map((header) => header.textContent);
    expect(headers).toEqual(["Name", "Created"]);
});

it("renders only the rows in view past the threshold", () => {
    // jsdom has no layout: give the scroll area a 400 px viewport.
    vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockReturnValue(400);
    vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockReturnValue(800);
    const count = VIRTUALIZE_AFTER + 400;
    render(
        <DataGrid
            rows={items(count)}
            columns={columns}
            getRowKey={(item) => item.id}
            caption="Items"
            refreshing
        />,
    );
    const table = screen.getByRole("table", { name: "Items" });
    // Every row is counted for assistive technology, but few are rendered.
    expect(table).toHaveAttribute("aria-rowcount", String(count + 1));
    expect(table).toHaveAttribute("aria-busy", "true");
    const rendered = within(table)
        .getAllByRole("row")
        .filter((row) => row.hasAttribute("data-index"));
    expect(rendered.length).toBeLessThan(VIRTUALIZE_AFTER);
    expect(rendered.length).toBeGreaterThan(0);
    expect(rendered[0]).toHaveAttribute("aria-rowindex", "2");
    vi.restoreAllMocks();
});

it("shows the empty notice instead of an empty grid", () => {
    render(
        <DataGrid
            rows={[]}
            columns={columns}
            getRowKey={(item) => item.id}
            caption="Items"
            empty={<p>No matches.</p>}
        />,
    );
    expect(screen.getByText("No matches.")).toBeInTheDocument();
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
});
