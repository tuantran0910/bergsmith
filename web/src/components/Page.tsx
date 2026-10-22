// Page building blocks shared by every page: the header, loading
// placeholders, empty states, and the "load more" footer of a list.

import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";

import { Button } from "../ui/Button";
import { Skeleton } from "../ui/Skeleton";
import { cn } from "../ui/cn";

/** The page's title, an optional explanation, and its main actions. */
export function PageHeader({
    title,
    description,
    actions,
}: {
    title: ReactNode;
    description?: ReactNode;
    actions?: ReactNode;
}) {
    return (
        <header className="mb-6 flex flex-wrap items-start justify-between gap-4">
            <div className="grid min-w-0 gap-1">
                <h1 className="m-0 text-2xl font-semibold tracking-tight break-words">
                    {title}
                </h1>
                {description !== undefined && (
                    <div className="text-sm text-muted-foreground">
                        {description}
                    </div>
                )}
            </div>
            {actions !== undefined && (
                <div className="flex flex-wrap items-center gap-2">
                    {actions}
                </div>
            )}
        </header>
    );
}

/**
 * Announces that content is loading and shows placeholder rows in its place.
 * The label is read by assistive technology; the rows are hidden from it.
 */
export function LoadingRows({
    label,
    rows = 5,
}: {
    label: string;
    rows?: number;
}) {
    return (
        <div role="status" className="grid gap-2">
            <span className="sr-only">{label}</span>
            <Skeleton className="h-9 w-full" />
            {Array.from({ length: rows }, (_, row) => (
                <Skeleton key={row} className="h-11 w-full" />
            ))}
        </div>
    );
}

/** Placeholders for a detail page: a title, a row of cards, and a block. */
export function LoadingDetail({ label }: { label: string }) {
    return (
        <div role="status" className="grid gap-6">
            <span className="sr-only">{label}</span>
            <Skeleton className="h-8 w-1/2 max-w-sm" />
            <div className="grid gap-4 sm:grid-cols-2">
                <Skeleton className="h-24" />
                <Skeleton className="h-24" />
            </div>
            <Skeleton className="h-48" />
        </div>
    );
}

/** What an empty list means and, when there is one, what to do about it. */
export function EmptyState({
    icon: Icon,
    title,
    children,
    className,
}: {
    icon: LucideIcon;
    title: string;
    children?: ReactNode;
    className?: string;
}) {
    return (
        <div
            className={cn(
                "flex flex-col items-center gap-2 rounded-xl border border-dashed px-6 py-12 text-center",
                className,
            )}
        >
            <span className="flex size-10 items-center justify-center rounded-full bg-muted text-muted-foreground">
                <Icon aria-hidden="true" className="size-5" />
            </span>
            <p className="m-0 font-medium">{title}</p>
            {children !== undefined && (
                <div className="max-w-md text-sm text-muted-foreground">
                    {children}
                </div>
            )}
        </div>
    );
}

/** A list's footer: how many rows are shown and, if more exist, a button. */
export function LoadMore({
    shown,
    noun,
    hasMore,
    loading,
    onLoad,
}: {
    shown: number;
    noun: string;
    hasMore: boolean;
    loading: boolean;
    onLoad: () => void;
}) {
    return (
        <div className="mt-3 flex flex-wrap items-center justify-between gap-3 text-sm text-muted-foreground">
            <span>
                {hasMore ? `${shown} shown, more available` : `${shown} shown`}
            </span>
            {hasMore && (
                <Button
                    variant="outline"
                    size="sm"
                    disabled={loading}
                    onClick={onLoad}
                >
                    {`Load more ${noun}`}
                </Button>
            )}
        </div>
    );
}

/** The bordered surface a data grid sits on. */
export function GridFrame({ children }: { children: ReactNode }) {
    return (
        <div className="overflow-hidden rounded-xl bg-card ring-1 ring-foreground/10">
            {children}
        </div>
    );
}
