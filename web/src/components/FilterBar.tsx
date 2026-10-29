// A list's filter bar (ADR 0056): a labeled search field, the page's own
// filters, a clear action, and a result summary that is announced when it
// changes. Filter state belongs to the page (and its URL), not this component.
//
// Below the small breakpoint the filters open in a bottom drawer (BS-032.T4);
// the controls render exactly once, in the drawer, so a mobile session never
// carries the inline copy in the accessibility tree.
//
// A server-filtered list passes `delay`: the field keeps its own text and
// reports it after a pause in typing, so a request is not sent per keystroke
// (ADR 0067).

import { SlidersHorizontalIcon, SearchIcon, XIcon } from "lucide-react";
import { useEffect, useId, useRef, useState, type ReactNode } from "react";

import { MAX_SEARCH } from "./listSearch";

import { Button } from "../ui/Button";
import { cn } from "../ui/cn";
import {
    Dialog,
    DialogClose,
    DialogContent,
    DialogDescription,
    DialogFooter,
    DialogHeader,
    DialogTitle,
} from "../ui/Dialog";
import { Input } from "../ui/Input";
import { useMediaQuery } from "./useMediaQuery";

/** A labeled search field with room for filters and a result summary. */
export function FilterBar({
    query,
    onQueryChange,
    queryLabel,
    placeholder,
    children,
    onClear,
    result,
    delay,
    tools,
    pills,
}: {
    query: string;
    onQueryChange: (value: string) => void;
    /** The visible purpose of the search field; also its accessible name. */
    queryLabel: string;
    placeholder?: string;
    /** Filter controls, for example selects. */
    children?: ReactNode;
    /** Clears every active filter; omit when no filter is active. */
    onClear?: (() => void) | undefined;
    /** For example `3 of 10 tables`; announced politely when it changes. */
    result?: ReactNode;
    /** Milliseconds of quiet typing before `onQueryChange`; omit to report each change. */
    delay?: number;
    /** Controls that stay inline at every width, such as the column menu. */
    tools?: ReactNode;
    /** The active filters, as removable pills under the bar. */
    pills?: ReactNode;
}) {
    const id = useId();
    // The field's own text. It follows `query` when the page changes it (for
    // example "Clear filters"), but not when the change is its own report.
    const [draft, setDraft] = useState(query);
    const [reported, setReported] = useState(query);
    const [seen, setSeen] = useState(query);
    if (query !== seen) {
        setSeen(query);
        if (query !== reported) {
            setReported(query);
            setDraft(query);
        }
    }
    const report = useRef(onQueryChange);
    useEffect(() => {
        report.current = onQueryChange;
    });
    useEffect(() => {
        if (delay === undefined || draft === reported) {
            return undefined;
        }
        const timer = window.setTimeout(() => {
            setReported(draft);
            report.current(draft);
        }, delay);
        return () => {
            window.clearTimeout(timer);
        };
    }, [draft, reported, delay]);
    const search = useRef<HTMLInputElement>(null);
    const [filtersOpen, setFiltersOpen] = useState(false);
    const narrow = !useMediaQuery("(min-width: 40rem)");

    // The drawer exists only below the breakpoint. When the layout widens while
    // it is open, close it and return focus to the search field, the one
    // filter control both layouts share; the Filters button has unmounted.
    useEffect(() => {
        if (narrow) {
            return;
        }
        setFiltersOpen((wasOpen) => {
            if (wasOpen) {
                queueMicrotask(() => {
                    search.current?.focus();
                });
            }
            return false;
        });
    }, [narrow]);

    const resultSummary =
        result === undefined ? null : (
            <p className="m-0 text-sm text-muted-foreground" aria-live="polite">
                {result}
            </p>
        );

    return (
        <div className="mb-4 grid gap-2">
            <div className="flex flex-wrap items-center gap-2">
                <label className="sr-only" htmlFor={id}>
                    {queryLabel}
                </label>
                <div className="relative min-w-0 flex-1 sm:max-w-xs">
                    <SearchIcon
                        aria-hidden="true"
                        className="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground"
                    />
                    <Input
                        ref={search}
                        id={id}
                        type="search"
                        value={draft}
                        maxLength={MAX_SEARCH}
                        placeholder={placeholder ?? queryLabel}
                        onChange={(event) => {
                            const value = event.target.value;
                            setDraft(value);
                            if (delay === undefined) {
                                setReported(value);
                                onQueryChange(value);
                            }
                        }}
                        className="pl-8"
                    />
                </div>
                {narrow && children !== undefined && (
                    <Button
                        variant="outline"
                        size="sm"
                        aria-haspopup="dialog"
                        onClick={() => {
                            setFiltersOpen(true);
                        }}
                    >
                        <SlidersHorizontalIcon aria-hidden="true" />
                        Filters
                    </Button>
                )}
                {!narrow && children}
                {onClear !== undefined && (
                    <Button variant="ghost" size="sm" onClick={onClear}>
                        <XIcon aria-hidden="true" />
                        Clear filters
                    </Button>
                )}
                {result !== undefined && !(narrow && filtersOpen) && (
                    <div className="ml-auto">{resultSummary}</div>
                )}
                {tools !== undefined && (
                    <div
                        className={cn(
                            "flex items-center gap-2",
                            (result === undefined || (narrow && filtersOpen)) &&
                                "ml-auto",
                        )}
                    >
                        {tools}
                    </div>
                )}
            </div>
            {pills}
            {narrow && children !== undefined && (
                <Dialog open={filtersOpen} onOpenChange={setFiltersOpen}>
                    <DialogContent
                        className="top-auto! bottom-0! left-0! max-h-[85dvh]! max-w-none! translate-x-0! translate-y-0! rounded-b-none!"
                        aria-label="Filters"
                    >
                        <DialogHeader>
                            <DialogTitle>Filters</DialogTitle>
                            <DialogDescription>
                                Changes apply as you choose them.
                            </DialogDescription>
                        </DialogHeader>
                        <div className="grid gap-3">{children}</div>
                        <DialogFooter>
                            <div className="sm:mr-auto">{resultSummary}</div>
                            <DialogClose render={<Button variant="outline" />}>
                                Show results
                            </DialogClose>
                        </DialogFooter>
                    </DialogContent>
                </Dialog>
            )}
        </div>
    );
}
