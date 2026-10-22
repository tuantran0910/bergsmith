// Adapted from shadcn/ui (base-nova style, MIT).

import type { ComponentProps } from "react";

import { cn } from "./cn";

/** A raised surface that groups related content. */
export function Card({ className, ...props }: ComponentProps<"div">) {
    return (
        <div
            data-slot="card"
            className={cn(
                "flex flex-col gap-4 overflow-hidden rounded-xl bg-card py-4 text-sm text-card-foreground ring-1 ring-foreground/10",
                className,
            )}
            {...props}
        />
    );
}

/** The card's title row. */
export function CardHeader({ className, ...props }: ComponentProps<"div">) {
    return (
        <div
            data-slot="card-header"
            className={cn(
                "grid auto-rows-min items-start gap-1 px-4",
                className,
            )}
            {...props}
        />
    );
}

/** The card's title, a level-two heading. */
export function CardTitle({
    className,
    children,
    ...props
}: ComponentProps<"h2">) {
    return (
        <h2
            data-slot="card-title"
            className={cn("text-base leading-snug font-medium", className)}
            {...props}
        >
            {children}
        </h2>
    );
}

/** Secondary text under the title. */
export function CardDescription({ className, ...props }: ComponentProps<"p">) {
    return (
        <p
            data-slot="card-description"
            className={cn("text-sm text-muted-foreground", className)}
            {...props}
        />
    );
}

/** The card's body. */
export function CardContent({ className, ...props }: ComponentProps<"div">) {
    return (
        <div
            data-slot="card-content"
            className={cn("px-4", className)}
            {...props}
        />
    );
}

/** A footer row, usually for actions. */
export function CardFooter({ className, ...props }: ComponentProps<"div">) {
    return (
        <div
            data-slot="card-footer"
            className={cn(
                "-mb-4 flex items-center gap-2 border-t bg-muted/50 p-4",
                className,
            )}
            {...props}
        />
    );
}
