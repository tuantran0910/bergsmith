// A list's data grid (ADR 0056): typed columns, controlled sorting, and one
// place for the shared table primitives. Pages own filter state; this owns the
// semantic header, sort affordance, and row rendering.

import { ArrowDownIcon, ArrowUpIcon, ChevronsUpDownIcon } from "lucide-react";
import { useMemo, type ReactNode } from "react";

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
    /** A comparable value makes the column sortable. */
    sortValue?: (row: Row) => string | number;
    align?: "start" | "end";
    className?: string;
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
 * A sortable data table with a framed, horizontally scrollable surface.
 * `empty` replaces the grid when the page has no rows (for example after a
 * filter); the caller keeps loading and error states outside it.
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

    if (sorted.length === 0) {
        return <>{empty}</>;
    }

    return (
        <GridFrame>
            <Table className="min-w-[36rem]">
                {caption !== undefined && (
                    <TableCaption className="sr-only">{caption}</TableCaption>
                )}
                <TableHeader>
                    <TableRow>
                        {columns.map((column) => {
                            const sortable =
                                column.sortValue !== undefined &&
                                onSortChange !== undefined;
                            const active = sort?.column === column.id;
                            const direction = active ? sort.direction : null;
                            const SortIcon =
                                direction === "asc"
                                    ? ArrowUpIcon
                                    : direction === "desc"
                                      ? ArrowDownIcon
                                      : ChevronsUpDownIcon;
                            return (
                                <TableHead
                                    key={column.id}
                                    aria-sort={
                                        sortable
                                            ? direction === "asc"
                                                ? "ascending"
                                                : direction === "desc"
                                                  ? "descending"
                                                  : "none"
                                            : undefined
                                    }
                                    className={cn(
                                        "px-[var(--density-cell-x)]",
                                        column.align === "end" && "text-right",
                                        column.className,
                                    )}
                                >
                                    {sortable ? (
                                        <button
                                            type="button"
                                            className={cn(
                                                "inline-flex h-6 items-center gap-1 rounded-sm text-left font-medium outline-none select-none hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50",
                                                column.align === "end" &&
                                                    "flex-row-reverse",
                                            )}
                                            onClick={() => {
                                                onSortChange({
                                                    column: column.id,
                                                    direction:
                                                        active &&
                                                        sort.direction === "asc"
                                                            ? "desc"
                                                            : "asc",
                                                });
                                            }}
                                        >
                                            {column.header}
                                            <SortIcon
                                                aria-hidden="true"
                                                className={cn(
                                                    "size-3.5 shrink-0",
                                                    !active && "opacity-40",
                                                )}
                                            />
                                        </button>
                                    ) : (
                                        column.header
                                    )}
                                </TableHead>
                            );
                        })}
                    </TableRow>
                </TableHeader>
                <TableBody>
                    {sorted.map((row) => (
                        <TableRow key={getRowKey(row)}>
                            {columns.map((column) => (
                                <TableCell
                                    key={column.id}
                                    className={cn(
                                        "px-[var(--density-cell-x)] py-[var(--density-cell-y)]",
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
