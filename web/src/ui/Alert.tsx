// Adapted from shadcn/ui (base-nova style, MIT), with status tones.

import { type VariantProps, cva } from "class-variance-authority";
import type { ComponentProps } from "react";

import { cn } from "./cn";

export const alertVariants = cva(
    "relative grid w-full gap-0.5 rounded-lg border border-l-4 bg-card px-3 py-2.5 text-left text-sm grid-cols-[0_1fr] has-[>svg]:grid-cols-[calc(var(--spacing)*4)_1fr] has-[>svg]:gap-x-2.5 *:[svg]:row-span-2 *:[svg]:translate-y-0.5 *:[svg]:text-current *:[svg:not([class*='size-'])]:size-4",
    {
        variants: {
            variant: {
                default: "border-l-primary",
                destructive: "border-l-destructive *:[svg]:text-destructive",
                warning: "border-l-warning *:[svg]:text-warning",
                success: "border-l-success *:[svg]:text-success",
                neutral: "border-l-border *:[svg]:text-muted-foreground",
            },
        },
        defaultVariants: {
            variant: "default",
        },
    },
);

export type AlertProps = ComponentProps<"div"> &
    VariantProps<typeof alertVariants>;

/**
 * A message that stays on the page. The caller chooses the live-region role:
 * `alert` for failures, `status` for outcomes, none for standing notes.
 */
export function Alert({
    className,
    variant = "default",
    ...props
}: AlertProps) {
    return (
        <div
            data-slot="alert"
            className={cn(alertVariants({ variant }), className)}
            {...props}
        />
    );
}

/** The alert's headline. */
export function AlertTitle({ className, ...props }: ComponentProps<"div">) {
    return (
        <div
            data-slot="alert-title"
            className={cn("col-start-2 font-medium", className)}
            {...props}
        />
    );
}

/** The alert's explanation and any actions. */
export function AlertDescription({
    className,
    ...props
}: ComponentProps<"div">) {
    return (
        <div
            data-slot="alert-description"
            className={cn(
                "col-start-2 grid justify-items-start gap-2 text-muted-foreground [&_p]:m-0 [&_p]:leading-relaxed",
                className,
            )}
            {...props}
        />
    );
}
