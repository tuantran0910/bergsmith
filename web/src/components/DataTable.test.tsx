import { fireEvent, render, screen } from "@testing-library/react";

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
