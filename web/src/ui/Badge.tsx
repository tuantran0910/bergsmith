// Adapted from shadcn/ui (base-nova style, MIT), with status tones.

import { type VariantProps, cva } from "class-variance-authority";
import type { ComponentProps } from "react";

import { cn } from "./cn";

export const badgeVariants = cva(
    "inline-flex h-5 w-fit shrink-0 items-center justify-center gap-1 overflow-hidden rounded-full border px-2 py-0.5 text-xs font-medium whitespace-nowrap [&>svg]:pointer-events-none [&>svg]:size-3",
    {
        variants: {
            variant: {
                default:
                    "border-transparent bg-primary text-primary-foreground",
                secondary:
                    "border-transparent bg-secondary text-secondary-foreground",
                outline: "border-border text-foreground",
                success: "border-success/30 bg-success/10 text-success",
                warning: "border-warning/30 bg-warning/10 text-warning",
                destructive:
                    "border-destructive/30 bg-destructive/10 text-destructive",
                info: "border-primary/30 bg-primary/10 text-primary",
                neutral: "border-border bg-muted text-muted-foreground",
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
