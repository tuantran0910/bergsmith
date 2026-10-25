// The names-first identity of a resource (ADR 0056): a primary name, optional
// context, a copy affordance, and technical details kept behind a disclosure.

import { ChevronRightIcon } from "lucide-react";
import type { ReactNode } from "react";

import { cn } from "../ui/cn";
import { CopyButton } from "./ResourceId";

/**
 * A resource's display name and its secondary identity. The name is a node so
 * a page can link it with its typed route; copy and details stay here so every
 * resource presents IDs the same way.
 */
export function ResourceLabel({
    name,
    context,
    copy,
    details,
    className,
}: {
    name: ReactNode;
    context?: ReactNode;
    copy?: { value: string; label: string };
    details?: ReactNode;
    className?: string;
}) {
    return (
        <div className={cn("grid min-w-0 gap-0.5", className)}>
            <span className="flex min-w-0 flex-wrap items-center gap-1 font-medium">
                {name}
                {copy !== undefined && (
                    <CopyButton value={copy.value} label={copy.label} />
                )}
            </span>
            {context !== undefined && (
                <span
                    className="line-clamp-1 text-xs text-muted-foreground"
                    title={typeof context === "string" ? context : undefined}
                >
                    {context}
                </span>
            )}
            {details !== undefined && (
                <details className="group text-xs text-muted-foreground">
                    <summary className="inline-flex min-h-6 w-fit cursor-pointer list-none items-center gap-0.5 rounded-sm outline-none select-none hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50 [&::-webkit-details-marker]:hidden">
                        <ChevronRightIcon
                            aria-hidden="true"
                            className="size-3 transition-transform group-open:rotate-90 motion-reduce:transition-none"
                        />
                        Technical details
                    </summary>
                    <div className="grid gap-1 pt-1">{details}</div>
                </details>
            )}
        </div>
    );
}
