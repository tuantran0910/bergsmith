// Adapted from shadcn/ui (base-nova style, MIT).

import type { ComponentProps, Ref } from "react";

import { cn } from "./cn";

/**
 * A data table that scrolls horizontally inside its container when narrow.
 * The scroll region is keyboard reachable and named by `containerLabel`.
 */
export function Table({
    className,
    containerLabel = "Table",
    containerRef,
    containerClassName,
    ...props
}: ComponentProps<"table"> & {
    containerLabel?: string;
    /** The scroll container, for a virtualizer or an overflow check. */
    containerRef?: Ref<HTMLDivElement>;
    containerClassName?: string;
}) {
    return (
        <div
            ref={containerRef}
            data-slot="table-container"
            role="region"
            aria-label={containerLabel}
            // A horizontally scrollable data table must be reachable by
            // keyboard (WCAG 2.2 AA); the inset outline stays visible inside
            // the frame's overflow clipping.
            // eslint-disable-next-line jsx-a11y-x/no-noninteractive-tabindex
            tabIndex={0}
            className={cn(
                "relative w-full overflow-x-auto focus-visible:outline-2 focus-visible:outline-offset-[-2px] focus-visible:outline-ring",
                containerClassName,
            )}
        >
            <table
                data-slot="table"
                className={cn(
                    "w-full caption-bottom text-sm tabular-nums",
                    className,
                )}
                {...props}
            />
        </div>
    );
}

export function TableHeader({ className, ...props }: ComponentProps<"thead">) {
    return (
        <thead
            data-slot="table-header"
            className={cn("[&_tr]:border-b", className)}
            {...props}
        />
    );
}

export function TableBody({ className, ...props }: ComponentProps<"tbody">) {
    return (
        <tbody
            data-slot="table-body"
            className={cn("[&_tr:last-child]:border-0", className)}
            {...props}
        />
    );
}

export function TableRow({ className, ...props }: ComponentProps<"tr">) {
    return (
        <tr
            data-slot="table-row"
            className={cn(
                "border-b transition-colors hover:bg-muted/50 data-[state=selected]:bg-muted",
                className,
            )}
            {...props}
        />
    );
}

/** A header cell; defaults to a column header for assistive technology. */
export function TableHead({
    className,
    scope = "col",
    ...props
}: ComponentProps<"th">) {
    return (
        <th
            data-slot="table-head"
            scope={scope}
            className={cn(
                "h-10 px-[var(--density-cell-x)] py-2 text-left align-middle font-semibold whitespace-nowrap text-muted-foreground",
                className,
            )}
            {...props}
        />
    );
}

export function TableCell({ className, ...props }: ComponentProps<"td">) {
    return (
        <td
            data-slot="table-cell"
            className={cn(
                "px-[var(--density-cell-x)] py-[var(--density-cell-y)] align-middle",
                className,
            )}
            {...props}
        />
    );
}

export function TableCaption({
    className,
    ...props
}: ComponentProps<"caption">) {
    return (
        <caption
            data-slot="table-caption"
            className={cn("mt-4 text-sm text-muted-foreground", className)}
            {...props}
        />
    );
}
