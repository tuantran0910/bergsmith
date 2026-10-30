// Summary figures and definition lists shared by detail pages (ADR 0056).

import { MinusIcon, TrendingDownIcon, TrendingUpIcon } from "lucide-react";
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
 * How a figure moved: a signed difference from `format.change` (`null` when
 * it held) and the window it is compared with. Direction is told in words and
 * an arrow, never color, which stays reserved for status.
 */
export interface MetricChange {
    difference: string | null;
    against: string;
}

function Change({ difference, against }: MetricChange) {
    const Icon =
        difference === null
            ? MinusIcon
            : difference.startsWith("+")
              ? TrendingUpIcon
              : TrendingDownIcon;
    return (
        <dd className="m-0 mt-1 flex items-center gap-1 text-xs text-muted-foreground">
            <Icon aria-hidden="true" className="size-3.5 shrink-0" />
            {difference === null
                ? `Same as ${against}`
                : `${difference} vs ${against}`}
        </dd>
    );
}

/**
 * One labeled figure. `detail` carries the qualifier (a unit, a scope, or a
 * relative time) so the headline number stays comparable across the strip;
 * `change` compares it with an earlier window.
 */
export function Metric({
    label,
    value,
    detail,
    change,
}: {
    label: string;
    value: ReactNode;
    detail?: ReactNode;
    change?: MetricChange;
}) {
    return (
        <div className="rounded-xl bg-card px-4 py-3 ring-1 ring-foreground/10">
            <dt className="text-xs font-semibold text-muted-foreground">
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
            {change !== undefined && <Change {...change} />}
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
