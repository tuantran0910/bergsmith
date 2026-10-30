// Page building blocks shared by every page: the header, loading
// placeholders, empty states, and the "load more" footer of a list.

import { SearchXIcon, type LucideIcon } from "lucide-react";
import { type ReactNode, useEffect, useRef } from "react";

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
            <div className="grid min-w-0 flex-1 basis-72 gap-1">
                <h1 className="m-0 text-xl font-semibold tracking-tight break-words">
                    {title}
                </h1>
                {description !== undefined && (
                    <div className="text-sm text-muted-foreground">
                        {description}
                    </div>
                )}
            </div>
            {actions !== undefined && (
                // A bare link among the actions is a 24 px target (WCAG
                // 2.5.8), like the buttons beside it.
                <div className="flex max-w-full flex-wrap items-center gap-2 [&>a]:inline-flex [&>a]:min-h-6 [&>a]:items-center">
                    {actions}
                </div>
            )}
        </header>
    );
}

/**
 * A detail page's header (ADR 0067): the title with its status, a line of
 * identifying facts, the page's actions, and its tabs. From `md` up it sticks
 * under the top bar, so state and actions stay in reach on a long tab. Its
 * height is published as `--detail-header-height`, which the document's
 * scroll padding adds, so a focused control never scrolls in under it.
 */
export function DetailHeader({
    title,
    status,
    meta,
    actions,
    tabs,
}: {
    title: ReactNode;
    /** Badges beside the title. */
    status?: ReactNode;
    meta?: ReactNode;
    actions?: ReactNode;
    /** The page's `TabsList`; the header renders inside its `Tabs`. */
    tabs: ReactNode;
}) {
    const ref = useRef<HTMLElement>(null);
    useEffect(() => {
        const header = ref.current;
        if (header === null || typeof ResizeObserver !== "function") {
            return;
        }
        const root = document.documentElement.style;
        const observer = new ResizeObserver(() => {
            root.setProperty(
                "--detail-header-height",
                `${header.offsetHeight}px`,
            );
        });
        observer.observe(header);
        return () => {
            observer.disconnect();
            root.removeProperty("--detail-header-height");
        };
    }, []);
    return (
        <header
            ref={ref}
            data-slot="detail-header"
            className="z-20 -mx-4 bg-background px-4 md:sticky md:top-(--header-height) md:-mx-6 md:px-6"
        >
            <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-3 pt-1 pb-4">
                <div className="grid min-w-0 flex-1 basis-72 gap-1">
                    <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1">
                        <h1 className="m-0 min-w-0 text-xl font-semibold tracking-tight break-words">
                            {title}
                        </h1>
                        {status}
                    </div>
                    {meta !== undefined && (
                        <div className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 text-sm text-muted-foreground">
                            {meta}
                        </div>
                    )}
                </div>
                {actions !== undefined && (
                    <div className="flex max-w-full flex-wrap items-center gap-2 [&>a]:inline-flex [&>a]:min-h-6 [&>a]:items-center">
                        {actions}
                    </div>
                )}
            </div>
            {tabs}
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
    action,
    className,
}: {
    icon: LucideIcon;
    title: string;
    children?: ReactNode;
    /** The next step, shown under the explanation. */
    action?: ReactNode;
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
            <p className="m-0 font-semibold">{title}</p>
            {children !== undefined && (
                <div className="max-w-md text-sm text-muted-foreground">
                    {children}
                </div>
            )}
            {action !== undefined && <div className="mt-1">{action}</div>}
        </div>
    );
}

/** The empty result of filters, with the way back to the full list. */
export function NoResults({
    onClear,
    children,
}: {
    onClear?: (() => void) | undefined;
    children?: ReactNode;
}) {
    return (
        <EmptyState
            icon={SearchXIcon}
            title="No matches."
            className="py-8"
            action={
                onClear === undefined ? undefined : (
                    <Button variant="outline" size="sm" onClick={onClear}>
                        Clear filters
                    </Button>
                )
            }
        >
            {children}
        </EmptyState>
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
