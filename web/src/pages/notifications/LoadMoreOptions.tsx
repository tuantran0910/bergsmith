// A selector fed by a cursor list can only offer what its pages hold. This
// exposes the next page explicitly, with its loading and failure states, so a
// destination or rule beyond the first page stays reachable.

import { Button } from "../../ui/Button";

export function LoadMoreOptions({
    noun,
    pending,
    loading,
    hasMore,
    error,
    onLoad,
}: {
    /** Plural noun, for example "destinations". */
    noun: string;
    pending: boolean;
    loading: boolean;
    hasMore: boolean;
    error: unknown;
    onLoad: () => void;
}) {
    if (!pending && !hasMore && error === null) return null;
    return (
        <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
            {pending && <span role="status">Loading {noun}…</span>}
            {hasMore && (
                <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    disabled={loading}
                    onClick={onLoad}
                >
                    {loading ? `Loading more ${noun}…` : `Load more ${noun}`}
                </Button>
            )}
            {error !== null && (
                <span role="alert" className="text-destructive">
                    Could not load more {noun}.
                    <Button
                        type="button"
                        variant="ghost"
                        size="sm"
                        disabled={loading}
                        onClick={onLoad}
                    >
                        Retry
                    </Button>
                </span>
            )}
        </div>
    );
}
