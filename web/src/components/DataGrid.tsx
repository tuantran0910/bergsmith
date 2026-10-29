// A filtered list's grid (ADR 0067): the data table for long, server-filtered
// lists. TanStack Table holds the column model and visibility; the page owns
// filters, sort, and paging, so the grid never filters rows itself.
//
// - The header sticks inside the grid's own scroll area, which is capped to
//   the viewport, so it stays in view however long the list grows.
// - Past `VIRTUALIZE_AFTER` rows only the rows in view (and a margin) are in
//   the document. Spacer rows keep the scroll height; their heights are set
//   through CSSOM, which the CSP allows, never a `style` attribute.
// - Arrow keys, Home, and End move between rows' first links, so a keyboard
//   walks the list without tabbing through every cell.
// - While a new filter loads, the previous rows stay and a bar reports it,
//   without moving the layout.
// - Below the small breakpoint the rows render as cards, like `DataTable`.

import {
    getCoreRowModel,
    useReactTable,
    type ColumnDef,
    type VisibilityState,
} from "@tanstack/react-table";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
    useCallback,
    useLayoutEffect,
    useMemo,
    useRef,
    useState,
    type KeyboardEvent,
    type ReactNode,
} from "react";

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
import {
    ariaSort,
    ColumnHeader,
    DataCards,
    sortRows,
    type DataTableColumn,
    type SortState,
} from "./DataTable";
import { GridFrame } from "./Page";
import { useMediaQuery } from "./useMediaQuery";

/** Lists longer than this render only the rows in view. */
export const VIRTUALIZE_AFTER = 100;

/** A row's height before it is measured: one line at comfortable density. */
const ESTIMATED_ROW_PX = 41;

/** Rows kept rendered beyond each edge of the viewport. */
const OVERSCAN = 10;

/** A virtualized list's top or bottom filler, sized through CSSOM. */
function SpacerRow({ height, columns }: { height: number; columns: number }) {
    const ref = useRef<HTMLTableRowElement>(null);
    useLayoutEffect(() => {
        if (ref.current !== null) {
            ref.current.style.height = `${height}px`;
        }
    }, [height]);
    if (height <= 0) {
        return null;
    }
    return (
        <tr ref={ref} aria-hidden="true">
            <td colSpan={columns} className="p-0" />
        </tr>
    );
}

/** The row index to move to for a navigation key, or `undefined`. */
export function nextRow(
    key: string,
    current: number,
    count: number,
): number | undefined {
    switch (key) {
        case "ArrowDown":
            return Math.min(current + 1, count - 1);
        case "ArrowUp":
            return Math.max(current - 1, 0);
        case "Home":
            return 0;
        case "End":
            return count - 1;
        default:
            return undefined;
    }
}

/**
 * A sortable grid with a sticky header, column visibility, keyboard row
 * navigation, and virtualization for long lists. `empty` replaces it when
 * there are no rows; `refreshing` marks the rows as the previous result
 * while a new one loads.
 */
export function DataGrid<Row>({
    rows,
    columns,
    getRowKey,
    sort,
    onSortChange,
    columnVisibility,
    caption,
    empty,
    refreshing = false,
}: {
    rows: readonly Row[];
    columns: readonly DataTableColumn<Row>[];
    getRowKey: (row: Row) => string;
    sort?: SortState | undefined;
    onSortChange?: ((sort: SortState) => void) | undefined;
    /** Column ids mapped to `false` when hidden. */
    columnVisibility?: VisibilityState | undefined;
    caption: string;
    empty?: ReactNode;
    refreshing?: boolean;
}) {
    const sorted = useMemo(() => {
        const column = columns.find((item) => item.id === sort?.column);
        // A server-sorted column's rows arrive in order.
        return sort === undefined || column?.sortValue === undefined
            ? rows
            : sortRows(rows, columns, sort);
    }, [rows, columns, sort]);
    const narrow = !useMediaQuery("(min-width: 40rem)");

    const definitions = useMemo<ColumnDef<Row>[]>(
        () =>
            columns.map((column, index) => ({
                id: column.id,
                header: column.header,
                enableHiding: column.hideable ?? index > 0,
                meta: column,
            })),
        [columns],
    );
    // `enableHiding` only governs toggles, so a stored choice for a column
    // that may not hide (such as one saved before a release) is dropped.
    const shown = useMemo<VisibilityState>(
        () =>
            Object.fromEntries(
                Object.entries(columnVisibility ?? {}).filter(([id]) =>
                    definitions.some(
                        (column) =>
                            column.id === id && column.enableHiding === true,
                    ),
                ),
            ),
        [columnVisibility, definitions],
    );
    // The build does not run React Compiler, and the table instance is read
    // only during this render, never passed to a memoized child.
    // eslint-disable-next-line react-hooks/incompatible-library -- see above
    const table = useReactTable({
        data: sorted as Row[],
        columns: definitions,
        getCoreRowModel: getCoreRowModel(),
        getRowId: getRowKey,
        state: { columnVisibility: shown },
        manualSorting: true,
        manualFiltering: true,
    });
    const visible = table
        .getVisibleLeafColumns()
        .map((column) => column.columnDef.meta as DataTableColumn<Row>);
    const modelRows = table.getRowModel().rows;

    const [container, setContainer] = useState<HTMLDivElement | null>(null);
    const virtual = modelRows.length > VIRTUALIZE_AFTER;
    const virtualizer = useVirtualizer({
        count: modelRows.length,
        getScrollElement: () => container,
        estimateSize: () => ESTIMATED_ROW_PX,
        overscan: OVERSCAN,
        enabled: virtual,
        getItemKey: (index) => modelRows[index]?.id ?? index,
    });

    const focusRow = useCallback(
        (index: number, attempts = 3) => {
            const target = container?.querySelector<HTMLElement>(
                `tr[data-index="${index}"] a, tr[data-index="${index}"] button`,
            );
            if (target !== null && target !== undefined) {
                // Focus scrolls the row into view.
                target.focus();
            } else if (attempts > 0) {
                // The row renders once the virtualizer reaches it.
                requestAnimationFrame(() => {
                    focusRow(index, attempts - 1);
                });
            }
        },
        [container],
    );

    function onKeyDown(event: KeyboardEvent<HTMLTableSectionElement>) {
        if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) {
            return;
        }
        const element = event.target as HTMLElement;
        if (element.closest("input, select, textarea") !== null) {
            return;
        }
        const row = element.closest<HTMLElement>("tr[data-index]");
        const current = Number(row?.dataset["index"]);
        if (row === null || Number.isNaN(current)) {
            return;
        }
        const next = nextRow(event.key, current, modelRows.length);
        if (next === undefined || next === current) {
            return;
        }
        event.preventDefault();
        if (virtual) {
            virtualizer.scrollToIndex(next, { align: "auto" });
        }
        focusRow(next);
    }

    if (sorted.length === 0) {
        return <>{empty}</>;
    }

    if (narrow) {
        return (
            <DataCards
                rows={sorted}
                columns={visible}
                getRowKey={getRowKey}
                sort={sort}
                onSortChange={onSortChange}
                caption={caption}
            />
        );
    }

    const items = virtual
        ? virtualizer.getVirtualItems()
        : modelRows.map((_row, index) => ({ index, start: 0, end: 0 }));
    const first = items[0];
    const last = items[items.length - 1];
    const before = virtual && first !== undefined ? first.start : 0;
    const after =
        virtual && last !== undefined
            ? virtualizer.getTotalSize() - last.end
            : 0;

    return (
        <GridFrame>
            <div
                aria-hidden="true"
                className={cn(
                    "h-0.5 bg-foreground/30 transition-opacity duration-(--duration-fast)",
                    refreshing ? "animate-pulse opacity-100" : "opacity-0",
                )}
            />
            <Table
                containerRef={setContainer}
                containerLabel={caption}
                containerClassName="max-h-[calc(100dvh-var(--header-height)-10rem)] min-h-40 overflow-y-auto overscroll-contain"
                className="min-w-[36rem]"
                aria-rowcount={modelRows.length + 1}
                aria-busy={refreshing || undefined}
            >
                <TableCaption className="sr-only">{caption}</TableCaption>
                <TableHeader className="sticky top-0 z-[1] bg-card shadow-[inset_0_-1px_0_var(--color-border)]">
                    <TableRow
                        aria-rowindex={1}
                        className="hover:bg-transparent"
                    >
                        {visible.map((column) => (
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
                <TableBody onKeyDown={onKeyDown}>
                    <SpacerRow height={before} columns={visible.length} />
                    {items.map((item) => {
                        const row = modelRows[item.index];
                        if (row === undefined) {
                            return null;
                        }
                        return (
                            <TableRow
                                key={row.id}
                                data-index={item.index}
                                aria-rowindex={item.index + 2}
                                ref={
                                    virtual
                                        ? virtualizer.measureElement
                                        : undefined
                                }
                            >
                                {visible.map((column) => (
                                    <TableCell
                                        key={column.id}
                                        className={cn(
                                            column.align === "end" &&
                                                "text-right tabular-nums",
                                            column.className,
                                        )}
                                    >
                                        {column.cell(row.original)}
                                    </TableCell>
                                ))}
                            </TableRow>
                        );
                    })}
                    <SpacerRow height={after} columns={visible.length} />
                </TableBody>
            </Table>
        </GridFrame>
    );
}
