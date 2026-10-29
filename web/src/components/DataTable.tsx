// A list's data grid (ADR 0056): typed columns, controlled sorting, and one
// place for the shared table primitives. Pages own filter state; this owns the
// semantic header, sort affordance, and row rendering.
//
// Below the small breakpoint the same rows render as cards (BS-032.T4), so a
// phone page never scrolls sideways and row actions stay visible. Exactly one
// layout renders at a time; a browser without `matchMedia` keeps the table.

import { ArrowDownIcon, ArrowUpIcon, ChevronsUpDownIcon } from "lucide-react";
import { useId, useMemo, type ReactNode } from "react";

import { Button } from "../ui/Button";
import { cn } from "../ui/cn";
import {
    Table,
    TableBody,
    TableCaption,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "../ui/Table";
import { GridFrame } from "./Page";
import { useMediaQuery } from "./useMediaQuery";

export type SortDirection = "asc" | "desc";

/** The sorted column and direction; the column id must match a column. */
export interface SortState {
    column: string;
    direction: SortDirection;
}

export interface DataTableColumn<Row> {
    id: string;
    header: string;
    cell: (row: Row) => ReactNode;
    /** A comparable value makes the column sortable in the browser. */
    sortValue?: (row: Row) => string | number;
    /**
     * The server orders by this column: it is sortable, and the rows arrive
     * in order, so the table does not reorder them.
     */
    serverSort?: boolean;
    /** Whether the column menu may hide it; defaults to every column but the first. */
    hideable?: boolean;
    align?: "start" | "end";
    className?: string;
    /**
     * How the column appears in the narrow card layout. Defaults: the first
     * column is the card title, an empty-header end-aligned column holds the
     * card actions, and every other column is a labelled detail.
     */
    card?: "title" | "actions" | "detail" | "hidden";
}

function compare(left: string | number, right: string | number): number {
    if (typeof left === "number" && typeof right === "number") {
        return left - right;
    }
    return String(left).localeCompare(String(right), undefined, {
        numeric: true,
        sensitivity: "base",
    });
}

/** Whether the column offers a sort, in the browser or on the server. */
export function isSortable<Row>(column: DataTableColumn<Row>): boolean {
    return column.sortValue !== undefined || column.serverSort === true;
}

/** Rows in the requested order, with the original order as a stable tiebreak. */
export function sortRows<Row>(
    rows: readonly Row[],
    columns: readonly DataTableColumn<Row>[],
    sort: SortState,
): Row[] {
    const column = columns.find((item) => item.id === sort.column);
    if (column?.sortValue === undefined) {
        return [...rows];
    }
    const direction = sort.direction === "asc" ? 1 : -1;
    return rows
        .map((row, index) => ({ row, index, value: column.sortValue?.(row) }))
        .sort((left, right) => {
            if (left.value === undefined || right.value === undefined) {
                return left.index - right.index;
            }
            return (
                compare(left.value, right.value) * direction ||
                left.index - right.index
            );
        })
        .map((item) => item.row);
}

/**
 * A sortable data table with a framed, horizontally scrollable surface on
 * wide screens, and the same rows as labelled cards below the small
 * breakpoint. `empty` replaces either layout when the page has no rows (for
 * example after a filter); the caller keeps loading and error states outside
 * it.
 */
export function DataTable<Row>({
    rows,
    columns,
    getRowKey,
    sort,
    onSortChange,
    caption,
    empty,
}: {
    rows: readonly Row[];
    columns: readonly DataTableColumn<Row>[];
    getRowKey: (row: Row) => string;
    sort?: SortState;
    onSortChange?: (sort: SortState) => void;
    caption?: string;
    empty?: ReactNode;
}) {
    const sorted = useMemo(
        () => (sort === undefined ? [...rows] : sortRows(rows, columns, sort)),
        [rows, columns, sort],
    );
    const narrow = !useMediaQuery("(min-width: 40rem)");

    if (sorted.length === 0) {
        return <>{empty}</>;
    }

    if (narrow) {
        return (
            <DataCards
                rows={sorted}
                columns={columns}
                getRowKey={getRowKey}
                sort={sort}
                onSortChange={onSortChange}
                caption={caption}
            />
        );
    }

    return (
        <GridFrame>
            <Table
                className="min-w-[36rem]"
                containerLabel={caption ?? "Table"}
            >
                {caption !== undefined && (
                    <TableCaption className="sr-only">{caption}</TableCaption>
                )}
                <TableHeader>
                    <TableRow>
                        {columns.map((column) => (
                            <TableHead
                                key={column.id}
                                aria-sort={ariaSort(column, sort, onSortChange)}
                                className={cn(
                                    column.align === "end" && "text-right",
                                    column.className,
                                )}
                            >
                                <ColumnHeader
                                    column={column}
                                    sort={sort}
                                    onSortChange={onSortChange}
                                />
                            </TableHead>
                        ))}
                    </TableRow>
                </TableHeader>
                <TableBody>
                    {sorted.map((row) => (
                        <TableRow key={getRowKey(row)}>
                            {columns.map((column) => (
                                <TableCell
                                    key={column.id}
                                    className={cn(
                                        column.align === "end" &&
                                            "text-right tabular-nums",
                                        column.className,
                                    )}
                                >
                                    {column.cell(row)}
                                </TableCell>
                            ))}
                        </TableRow>
                    ))}
                </TableBody>
            </Table>
        </GridFrame>
    );
}

/** A column header's `aria-sort`, or `undefined` when it cannot sort. */
export function ariaSort<Row>(
    column: DataTableColumn<Row>,
    sort: SortState | undefined,
    onSortChange: ((sort: SortState) => void) | undefined,
): "ascending" | "descending" | "none" | undefined {
    if (!isSortable(column) || onSortChange === undefined) {
        return undefined;
    }
    if (sort?.column !== column.id) {
        return "none";
    }
    return sort.direction === "asc" ? "ascending" : "descending";
}

/** A column's header text, as a sort toggle when the column can sort. */
export function ColumnHeader<Row>({
    column,
    sort,
    onSortChange,
}: {
    column: DataTableColumn<Row>;
    sort: SortState | undefined;
    onSortChange: ((sort: SortState) => void) | undefined;
}) {
    if (!isSortable(column) || onSortChange === undefined) {
        return <>{column.header}</>;
    }
    const active = sort?.column === column.id;
    const direction = active ? sort.direction : null;
    const SortIcon =
        direction === "asc"
            ? ArrowUpIcon
            : direction === "desc"
              ? ArrowDownIcon
              : ChevronsUpDownIcon;
    return (
        <button
            type="button"
            className={cn(
                "inline-flex h-6 items-center gap-1 rounded-sm text-left font-semibold outline-none select-none hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50",
                column.align === "end" && "flex-row-reverse",
            )}
            onClick={() => {
                onSortChange({
                    column: column.id,
                    direction:
                        active && sort.direction === "asc" ? "desc" : "asc",
                });
            }}
        >
            {column.header}
            <SortIcon
                aria-hidden="true"
                className={cn("size-3.5 shrink-0", !active && "opacity-40")}
            />
        </button>
    );
}

/**
 * Rows as labelled cards, for the narrow layout. The first column is the
 * card title unless a column says otherwise (see `DataTableColumn.card`).
 */
export function DataCards<Row>({
    rows,
    columns,
    getRowKey,
    sort,
    onSortChange,
    caption,
}: {
    rows: readonly Row[];
    columns: readonly DataTableColumn<Row>[];
    getRowKey: (row: Row) => string;
    sort?: SortState | undefined;
    onSortChange?: ((sort: SortState) => void) | undefined;
    caption?: string | undefined;
}) {
    const roles = columns.map((column, index) => {
        if (column.card !== undefined) {
            return column.card;
        }
        if (index === 0) {
            return "title";
        }
        return column.header === "" && column.align === "end"
            ? "actions"
            : "detail";
    });
    const title = columns[roles.indexOf("title")];
    const actions = columns.filter(
        (_column, index) => roles[index] === "actions",
    );
    const details = columns.filter(
        (_column, index) => roles[index] === "detail",
    );
    return (
        <section aria-label={`${caption ?? "Table"} cards`}>
            <CardSort
                columns={columns}
                sort={sort}
                onSortChange={onSortChange}
            />
            <ul className="m-0 grid list-none gap-3 p-0">
                {rows.map((row) => (
                    <li key={getRowKey(row)}>
                        <article className="rounded-xl bg-card p-4 ring-1 ring-foreground/10">
                            <div className="flex flex-wrap items-start justify-between gap-2">
                                {title !== undefined && (
                                    <div className="min-w-0 flex-1 break-words">
                                        {title.cell(row)}
                                    </div>
                                )}
                                {actions.length > 0 && (
                                    <div className="flex flex-wrap items-center justify-end gap-2">
                                        {actions.map((column) => (
                                            <div key={column.id}>
                                                {column.cell(row)}
                                            </div>
                                        ))}
                                    </div>
                                )}
                            </div>
                            {details.length > 0 && (
                                <dl className="m-0 mt-3 grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1 text-sm">
                                    {details.map((column) => (
                                        <div
                                            key={column.id}
                                            className="contents"
                                        >
                                            <dt className="text-muted-foreground">
                                                {column.header}
                                            </dt>
                                            <dd className="m-0 min-w-0 break-words">
                                                {column.cell(row)}
                                            </dd>
                                        </div>
                                    ))}
                                </dl>
                            )}
                        </article>
                    </li>
                ))}
            </ul>
        </section>
    );
}

/** A compact sort control for the card layout, where the header is absent. */
function CardSort<Row>({
    columns,
    sort,
    onSortChange,
}: {
    columns: readonly DataTableColumn<Row>[];
    sort?: SortState | undefined;
    onSortChange?: ((sort: SortState) => void) | undefined;
}) {
    const id = useId();
    const sortable = columns.filter(isSortable);
    if (
        sort === undefined ||
        onSortChange === undefined ||
        sortable.length === 0
    ) {
        return null;
    }
    const descending = sort.direction === "desc";
    return (
        <div className="mb-2 flex items-center gap-2">
            <label className="sr-only" htmlFor={id}>
                Sort by
            </label>
            <select
                id={id}
                value={sort.column}
                onChange={(event) => {
                    onSortChange({
                        column: event.currentTarget.value,
                        direction: sort.direction,
                    });
                }}
                className="h-8 min-w-0 flex-1 rounded-lg border border-input bg-transparent px-2 text-sm outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50"
            >
                {sortable.map((column) => (
                    <option key={column.id} value={column.id}>
                        {column.header}
                    </option>
                ))}
            </select>
            <Button
                variant="outline"
                size="icon-sm"
                aria-label={descending ? "Sort ascending" : "Sort descending"}
                onClick={() => {
                    onSortChange({
                        column: sort.column,
                        direction: descending ? "asc" : "desc",
                    });
                }}
            >
                {descending ? (
                    <ArrowDownIcon aria-hidden="true" />
                ) : (
                    <ArrowUpIcon aria-hidden="true" />
                )}
            </Button>
        </div>
    );
}
