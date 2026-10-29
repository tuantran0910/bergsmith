import { act, fireEvent, render, screen, within } from "@testing-library/react";

import { DataTable, sortRows, type DataTableColumn } from "./DataTable";

interface Row {
    id: string;
    name: string;
    size: number;
}

const ROWS: Row[] = [
    { id: "b", name: "beta", size: 20 },
    { id: "a", name: "Alpha", size: 3 },
    { id: "c", name: "gamma", size: 10 },
];

const COLUMNS: DataTableColumn<Row>[] = [
    {
        id: "name",
        header: "Name",
        cell: (row) => row.name,
        sortValue: (row) => row.name,
    },
    {
        id: "size",
        header: "Size",
        align: "end",
        cell: (row) => row.size,
        sortValue: (row) => row.size,
    },
];

it("sorts by name without case and by number", () => {
    expect(
        sortRows(ROWS, COLUMNS, { column: "name", direction: "asc" }).map(
            (row) => row.id,
        ),
    ).toEqual(["a", "b", "c"]);
    expect(
        sortRows(ROWS, COLUMNS, { column: "name", direction: "desc" }).map(
            (row) => row.id,
        ),
    ).toEqual(["c", "b", "a"]);
    expect(
        sortRows(ROWS, COLUMNS, { column: "size", direction: "asc" }).map(
            (row) => row.id,
        ),
    ).toEqual(["a", "c", "b"]);
});

it("keeps the original order for an unknown sort column", () => {
    expect(
        sortRows(ROWS, COLUMNS, { column: "missing", direction: "asc" }).map(
            (row) => row.id,
        ),
    ).toEqual(["b", "a", "c"]);
});

it("renders a semantic table and reports the active sort", () => {
    render(
        <DataTable
            rows={ROWS}
            columns={COLUMNS}
            getRowKey={(row) => row.id}
            sort={{ column: "size", direction: "asc" }}
            onSortChange={() => undefined}
            caption="Example rows"
        />,
    );

    expect(
        screen.getByRole("table", { name: "Example rows" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: /Size/ })).toHaveAttribute(
        "aria-sort",
        "ascending",
    );
    expect(screen.getByRole("columnheader", { name: /Name/ })).toHaveAttribute(
        "aria-sort",
        "none",
    );
});

it("asks for a new direction when a sorted column is clicked again", () => {
    const onSortChange = vi.fn();
    render(
        <DataTable
            rows={ROWS}
            columns={COLUMNS}
            getRowKey={(row) => row.id}
            sort={{ column: "name", direction: "asc" }}
            onSortChange={onSortChange}
        />,
    );

    fireEvent.click(screen.getByRole("button", { name: /Name/ }));
    expect(onSortChange).toHaveBeenCalledWith({
        column: "name",
        direction: "desc",
    });

    fireEvent.click(screen.getByRole("button", { name: /Size/ }));
    expect(onSortChange).toHaveBeenLastCalledWith({
        column: "size",
        direction: "asc",
    });
});

it("shows the caller's empty state instead of an empty grid", () => {
    render(
        <DataTable
            rows={[]}
            columns={COLUMNS}
            getRowKey={(row) => row.id}
            empty={<p>No matches.</p>}
        />,
    );
    expect(screen.getByText("No matches.")).toBeInTheDocument();
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
});

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

function stubNarrow() {
    stubMedia(320);
}

const WITH_ACTIONS: DataTableColumn<Row>[] = [
    ...COLUMNS,
    {
        id: "actions",
        header: "",
        align: "end",
        cell: (row) => <button type="button">{`Edit ${row.name}`}</button>,
    },
];

it("renders the same rows as cards, with labels and visible actions", () => {
    stubNarrow();
    render(
        <DataTable
            rows={ROWS}
            columns={WITH_ACTIONS}
            getRowKey={(row) => row.id}
            sort={{ column: "name", direction: "asc" }}
            onSortChange={() => undefined}
            caption="Example rows"
        />,
    );

    const cards = screen.getByRole("region", { name: "Example rows cards" });
    expect(within(cards).getAllByRole("article")).toHaveLength(3);
    const terms = within(cards).getAllByRole("term");
    expect(terms).toHaveLength(3);
    expect(terms[0]).toHaveTextContent("Size");
    expect(
        within(cards).getByRole("button", { name: "Edit Alpha" }),
    ).toBeVisible();
});

it("offers the same ordering from the card layout", () => {
    stubNarrow();
    const onSortChange = vi.fn();
    render(
        <DataTable
            rows={ROWS}
            columns={COLUMNS}
            getRowKey={(row) => row.id}
            sort={{ column: "name", direction: "asc" }}
            onSortChange={onSortChange}
            caption="Example rows"
        />,
    );

    const cards = screen.getByRole("region", { name: "Example rows cards" });
    fireEvent.change(within(cards).getByRole("combobox", { name: "Sort by" }), {
        target: { value: "size" },
    });
    expect(onSortChange).toHaveBeenCalledWith({
        column: "size",
        direction: "asc",
    });
    fireEvent.click(
        within(cards).getByRole("button", { name: "Sort descending" }),
    );
    expect(onSortChange).toHaveBeenLastCalledWith({
        column: "name",
        direction: "desc",
    });
});

it("switches layouts exactly at the 40rem breakpoint", async () => {
    const media = stubMedia(639);
    render(
        <DataTable
            rows={ROWS}
            columns={COLUMNS}
            getRowKey={(row) => row.id}
            caption="Example rows"
        />,
    );
    expect(
        screen.getByRole("region", { name: "Example rows cards" }),
    ).toBeInTheDocument();
    expect(screen.queryByRole("table")).not.toBeInTheDocument();

    await act(async () => {
        media.setWidth(641);
        await Promise.resolve();
    });
    expect(
        screen.getByRole("table", { name: "Example rows" }),
    ).toBeInTheDocument();
    expect(
        screen.queryByRole("region", { name: "Example rows cards" }),
    ).not.toBeInTheDocument();
});
