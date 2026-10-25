// A disclosure for IDs, revisions, and raw values that should not lead a
// name-first detail page.

import { ChevronRightIcon } from "lucide-react";
import type { ReactNode } from "react";

import { cn } from "../ui/cn";
import { DefinitionList, type Definition } from "./Metric";

/** A consistent technical-details disclosure for detail pages. */
export function TechnicalDetails({
    items,
    children,
    className,
}: {
    items?: readonly Definition[];
    children?: ReactNode;
    className?: string;
}) {
    return (
        <details
            className={cn(
                "group border-t pt-3 text-xs text-muted-foreground",
                className,
            )}
        >
            <summary className="inline-flex min-h-6 w-fit cursor-pointer list-none items-center gap-0.5 rounded-sm outline-none select-none hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50 [&::-webkit-details-marker]:hidden">
                <ChevronRightIcon
                    aria-hidden="true"
                    className="size-3 transition-transform group-open:rotate-90 motion-reduce:transition-none"
                />
                Technical details
            </summary>
            {items === undefined ? (
                <div className="pt-1">{children}</div>
            ) : (
                <DefinitionList className="pt-2" items={items} />
            )}
        </details>
    );
}
