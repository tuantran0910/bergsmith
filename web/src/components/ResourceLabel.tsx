// The names-first identity of a resource (ADR 0056): a primary name, optional
// context, a copy affordance, and technical details kept behind a disclosure.

import type { ReactNode } from "react";

import { cn } from "../ui/cn";
import { CopyButton } from "./ResourceId";
import { TechnicalDetails } from "./TechnicalDetails";

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
                <TechnicalDetails className="border-0 pt-0">
                    {details}
                </TechnicalDetails>
            )}
        </div>
    );
}
