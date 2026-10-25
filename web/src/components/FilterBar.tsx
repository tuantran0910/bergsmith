// A list's filter bar (ADR 0056): a labeled search field, the page's own
// filters, a clear action, and a result summary that is announced when it
// changes. Filter state belongs to the page (and its URL), not this component.

import { SearchIcon, XIcon } from "lucide-react";
import { useId, type ReactNode } from "react";

import { Button } from "../ui/Button";
import { Input } from "../ui/Input";

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
    return (
        <div className="mb-4 flex flex-wrap items-center gap-2">
            <label className="sr-only" htmlFor={id}>
                {queryLabel}
            </label>
            <div className="relative min-w-52 flex-1 sm:max-w-xs">
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
            {children}
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
    );
}
