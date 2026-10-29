// A list's filter bar (ADR 0056): a labeled search field, the page's own
// filters, a clear action, and a result summary that is announced when it
// changes. Filter state belongs to the page (and its URL), not this component.
//
// Below the small breakpoint the filters open in a bottom drawer (BS-032.T4);
// the controls render exactly once, in the drawer, so a mobile session never
// carries the inline copy in the accessibility tree.

import { SlidersHorizontalIcon, SearchIcon, XIcon } from "lucide-react";
import { useId, useState, type ReactNode } from "react";

import { Button } from "../ui/Button";
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
}) {
    const id = useId();
    const [filtersOpen, setFiltersOpen] = useState(false);
    const narrow = !useMediaQuery("(min-width: 40rem)");
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
                        id={id}
                        type="search"
                        value={query}
                        placeholder={placeholder ?? queryLabel}
                        onChange={(event) => {
                            onQueryChange(event.target.value);
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
                {result !== undefined && (
                    <p
                        className="m-0 ml-auto text-sm text-muted-foreground"
                        aria-live="polite"
                    >
                        {result}
                    </p>
                )}
            </div>
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
