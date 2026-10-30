// Adapted from shadcn/ui (base-nova style, MIT). Base UI positions the popup
// through CSSOM properties, which the CSP allows (ADR 0067).

import { Popover as PopoverPrimitive } from "@base-ui/react/popover";

import { cn } from "./cn";

/**
 * A panel anchored to a trigger, for controls that need more room than a
 * menu, such as a filter list. Escape and an outside click close it, and
 * focus returns to the trigger. Pass `modal`: Base UI's non-modal focus
 * guards fail WCAG checks (ADR 0067).
 */
export function Popover(props: PopoverPrimitive.Root.Props) {
    return <PopoverPrimitive.Root {...props} />;
}

export function PopoverTrigger(props: PopoverPrimitive.Trigger.Props) {
    return <PopoverPrimitive.Trigger data-slot="popover-trigger" {...props} />;
}

export function PopoverContent({
    className,
    side = "bottom",
    align = "start",
    sideOffset = 4,
    anchor,
    ...props
}: PopoverPrimitive.Popup.Props &
    Pick<
        PopoverPrimitive.Positioner.Props,
        "align" | "side" | "sideOffset" | "anchor"
    >) {
    return (
        <PopoverPrimitive.Portal>
            <PopoverPrimitive.Positioner
                side={side}
                align={align}
                sideOffset={sideOffset}
                anchor={anchor}
                className="isolate z-50"
            >
                <PopoverPrimitive.Popup
                    data-slot="popover-content"
                    className={cn(
                        "max-h-(--available-height) w-72 origin-(--transform-origin) overflow-y-auto rounded-md border bg-popover p-3 text-sm text-popover-foreground shadow-md transition-[opacity,scale] duration-(--duration-fast) ease-(--ease-out) outline-none data-ending-style:scale-[0.97] data-ending-style:opacity-0 data-ending-style:ease-(--ease-in) data-starting-style:scale-[0.97] data-starting-style:opacity-0",
                        className,
                    )}
                    {...props}
                />
            </PopoverPrimitive.Positioner>
        </PopoverPrimitive.Portal>
    );
}

export function PopoverTitle({
    className,
    ...props
}: PopoverPrimitive.Title.Props) {
    return (
        <PopoverPrimitive.Title
            data-slot="popover-title"
            className={cn("m-0 text-sm font-semibold", className)}
            {...props}
        />
    );
}
