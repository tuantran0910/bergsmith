// Adapted from shadcn/ui (base-nova style, MIT), with status tones.

import { type VariantProps, cva } from "class-variance-authority";
import type { ComponentProps } from "react";

import { cn } from "./cn";

export const badgeVariants = cva(
    "inline-flex h-5 w-fit shrink-0 items-center justify-center gap-1 overflow-hidden rounded-full border px-2 py-0.5 text-xs font-semibold whitespace-nowrap [&>svg]:pointer-events-none [&>svg]:size-3",
    {
        variants: {
            variant: {
                // Neutral and quiet: a filled graphite pill would outweigh
                // the status tones, which are the only color (ADR 0067).
                default: "border-border bg-secondary text-foreground",
                secondary:
                    "border-transparent bg-secondary text-secondary-foreground",
                outline: "border-border text-foreground",
                success: "border-transparent bg-success-soft text-success",
                warning: "border-transparent bg-warning-soft text-warning",
                destructive:
                    "border-transparent bg-destructive-soft text-destructive",
                info: "border-transparent bg-info-soft text-info",
                neutral: "border-border bg-muted text-muted-foreground",
                running: "border-transparent bg-running-soft text-running",
            },
        },
        defaultVariants: {
            variant: "default",
        },
    },
);

export type BadgeProps = ComponentProps<"span"> &
    VariantProps<typeof badgeVariants>;

/** A short label, such as a status. Pair a tone with text, never color alone. */
export function Badge({
    className,
    variant = "default",
    ...props
}: BadgeProps) {
    return (
        <span
            data-slot="badge"
            className={cn(badgeVariants({ variant }), className)}
            {...props}
        />
    );
}
