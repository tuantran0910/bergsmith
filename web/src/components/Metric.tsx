// Summary figures and definition lists shared by detail pages (ADR 0056).

import type { ReactNode } from "react";

import { cn } from "../ui/cn";

/** The summary strip's list wrapper; pair it with `Metric` children. */
export function MetricGrid({
    children,
    className,
}: {
    children: ReactNode;
    className?: string;
}) {
    return (
        <dl
            className={cn(
                "m-0 grid grid-cols-2 gap-3 md:grid-cols-4",
                className,
            )}
        >
            {children}
        </dl>
    );
}

/**
 * One labeled figure. `detail` carries the qualifier (a unit, a scope, or a
 * relative time) so the headline number stays comparable across the strip.
 */
export function Metric({
    label,
    value,
    detail,
}: {
    label: string;
    value: ReactNode;
    detail?: ReactNode;
}) {
    return (
        <div className="rounded-xl bg-card px-4 py-3 ring-1 ring-foreground/10">
            <dt className="text-xs font-medium text-muted-foreground">
                {label}
            </dt>
            <dd className="m-0 mt-1 text-lg font-semibold break-words tabular-nums">
                {value}
            </dd>
            {detail !== undefined && (
                <dd className="m-0 text-xs break-words text-muted-foreground">
                    {detail}
                </dd>
            )}
        </div>
    );
}

/** A term/description pair rendered as a semantic description list. */
export interface Definition {
    term: ReactNode;
    description: ReactNode;
}

/** A compact, wrapping description list for details, IDs, and settings. */
export function DefinitionList({
    items,
    className,
}: {
    items: readonly Definition[];
    className?: string;
}) {
    return (
        <dl
            className={cn(
                "m-0 grid gap-x-6 gap-y-3 text-sm sm:grid-cols-[max-content_1fr]",
                className,
            )}
        >
            {items.map((item, index) => (
                <div
                    key={index}
                    className="grid gap-0.5 sm:col-span-2 sm:grid-cols-subgrid sm:gap-x-6 sm:gap-y-0"
                >
                    <dt className="text-muted-foreground">{item.term}</dt>
                    <dd className="m-0 min-w-0">{item.description}</dd>
                </div>
            ))}
        </dl>
    );
}
