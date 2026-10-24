// Adapted from shadcn/ui (base-nova style, MIT).

import { Button as ButtonPrimitive } from "@base-ui/react/button";
import { type VariantProps, cva } from "class-variance-authority";

import { cn } from "./cn";

export const buttonVariants = cva(
    "inline-flex shrink-0 items-center justify-center gap-1.5 rounded-lg border border-transparent text-sm font-medium whitespace-nowrap transition-colors outline-none select-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:pointer-events-none disabled:opacity-50 aria-invalid:border-destructive [&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-4",
    {
        variants: {
            variant: {
                default:
                    "bg-primary text-primary-foreground hover:bg-primary/95",
                outline:
                    "border-input bg-background hover:bg-muted aria-expanded:bg-muted",
                secondary:
                    "bg-secondary text-secondary-foreground hover:bg-secondary/80",
                ghost: "hover:bg-muted aria-expanded:bg-muted aria-pressed:bg-muted",
                destructive:
                    "bg-destructive/10 text-destructive hover:bg-destructive/20 focus-visible:border-destructive/40 focus-visible:ring-destructive/20",
                link: "text-primary underline-offset-4 hover:underline",
            },
            size: {
                default: "h-8 px-2.5",
                sm: "h-7 gap-1 rounded-md px-2.5 text-[0.8rem] [&_svg:not([class*='size-'])]:size-3.5",
                lg: "h-9 px-3",
                icon: "size-8",
                "icon-sm": "size-7 rounded-md",
            },
        },
        defaultVariants: {
            variant: "default",
            size: "default",
        },
    },
);

export type ButtonProps = ButtonPrimitive.Props &
    VariantProps<typeof buttonVariants>;

/** A button in one of the design system's variants and sizes. */
export function Button({
    className,
    variant = "default",
    size = "default",
    ...props
}: ButtonProps) {
    return (
        <ButtonPrimitive
            data-slot="button"
            className={cn(buttonVariants({ variant, size }), className)}
            {...props}
        />
    );
}
