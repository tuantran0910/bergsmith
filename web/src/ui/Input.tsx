// Adapted from shadcn/ui (base-nova style, MIT).

import type { ComponentProps } from "react";

import { cn } from "./cn";

/** A single-line text field. */
export function Input({ className, ...props }: ComponentProps<"input">) {
    return (
        <input
            data-slot="input"
            className={cn(
                "h-8 w-full min-w-0 rounded-lg border border-input bg-transparent px-2.5 py-1 text-sm transition-colors outline-none placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:pointer-events-none disabled:cursor-not-allowed disabled:opacity-50 aria-invalid:border-destructive aria-invalid:ring-3 aria-invalid:ring-destructive/20",
                className,
            )}
            {...props}
        />
    );
}

/** A field label; wrap the control or point at it with `htmlFor`. */
export function Label({ className, ...props }: ComponentProps<"label">) {
    return (
        // The caller supplies the control, either nested or through htmlFor.
        // eslint-disable-next-line jsx-a11y-x/label-has-associated-control
        <label
            data-slot="label"
            className={cn(
                "flex flex-col gap-1.5 text-sm leading-none font-semibold select-none",
                className,
            )}
            {...props}
        />
    );
}
