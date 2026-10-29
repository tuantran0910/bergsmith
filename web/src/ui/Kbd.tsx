import type { ComponentProps } from "react";

import { cn } from "./cn";

/** A key or key combination as the user types it, such as `Ctrl K`. */
export function Kbd({ className, ...props }: ComponentProps<"kbd">) {
    return (
        <kbd
            data-slot="kbd"
            className={cn(
                "inline-flex h-5 min-w-5 items-center justify-center gap-0.5 rounded-sm border bg-muted px-1 font-sans text-2xs text-muted-foreground",
                className,
            )}
            {...props}
        />
    );
}
