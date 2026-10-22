// Adapted from shadcn/ui (base-nova style, MIT).

import type { ComponentProps } from "react";

import { cn } from "./cn";

/** A placeholder block shown while content loads. Hidden from assistive technology. */
export function Skeleton({ className, ...props }: ComponentProps<"div">) {
    return (
        <div
            data-slot="skeleton"
            aria-hidden="true"
            className={cn(
                "animate-pulse rounded-md bg-muted motion-reduce:animate-none",
                className,
            )}
            {...props}
        />
    );
}
