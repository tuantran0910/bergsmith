// Adapted from shadcn/ui (base-nova style, MIT).

import { Tooltip as TooltipPrimitive } from "@base-ui/react/tooltip";

import { cn } from "./cn";

/** Shares one open delay across the tooltips inside it. */
export function TooltipProvider(props: TooltipPrimitive.Provider.Props) {
    return <TooltipPrimitive.Provider {...props} />;
}

/**
 * A short hint on hover or keyboard focus. It supplements a visible label or
 * an accessible name; never put required information only in a tooltip.
 */
export function Tooltip(props: TooltipPrimitive.Root.Props) {
    return <TooltipPrimitive.Root {...props} />;
}

export function TooltipTrigger(props: TooltipPrimitive.Trigger.Props) {
    return <TooltipPrimitive.Trigger data-slot="tooltip-trigger" {...props} />;
}

export function TooltipContent({
    className,
    side = "top",
    sideOffset = 4,
    align = "center",
    children,
    ...props
}: TooltipPrimitive.Popup.Props &
    Pick<TooltipPrimitive.Positioner.Props, "align" | "side" | "sideOffset">) {
    return (
        <TooltipPrimitive.Portal>
            <TooltipPrimitive.Positioner
                align={align}
                side={side}
                sideOffset={sideOffset}
                className="isolate z-50"
            >
                <TooltipPrimitive.Popup
                    data-slot="tooltip-content"
                    className={cn(
                        "w-fit max-w-xs origin-(--transform-origin) rounded-md bg-foreground px-3 py-1.5 text-xs text-background transition-[opacity,scale] duration-100 data-ending-style:scale-95 data-ending-style:opacity-0 data-starting-style:scale-95 data-starting-style:opacity-0 motion-reduce:transition-none",
                        className,
                    )}
                    {...props}
                >
                    {children}
                </TooltipPrimitive.Popup>
            </TooltipPrimitive.Positioner>
        </TooltipPrimitive.Portal>
    );
}
