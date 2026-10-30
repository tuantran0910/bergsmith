// Adapted from shadcn/ui (base-nova style, MIT); underline style with a
// sliding indicator (ADR 0067).

import { Tabs as TabsPrimitive } from "@base-ui/react/tabs";

import { cn } from "./cn";

/** Tabs that switch between panels; arrow keys move between tabs. */
export function Tabs({ className, ...props }: TabsPrimitive.Root.Props) {
    return (
        <TabsPrimitive.Root
            data-slot="tabs"
            className={cn("flex flex-col gap-4", className)}
            {...props}
        />
    );
}

/**
 * The tab row. It scrolls sideways when the tabs do not fit, and an
 * indicator slides under the active tab. The indicator's position comes from
 * CSS variables Base UI sets through the CSSOM, which the CSP allows.
 */
export function TabsList({
    className,
    children,
    ...props
}: TabsPrimitive.List.Props) {
    return (
        <TabsPrimitive.List
            data-slot="tabs-list"
            className={cn(
                "relative flex h-9 w-full items-stretch gap-4 overflow-x-auto border-b text-muted-foreground",
                className,
            )}
            {...props}
        >
            {children}
            <TabsPrimitive.Indicator
                data-slot="tabs-indicator"
                className="absolute bottom-0 left-0 h-0.5 w-(--active-tab-width) translate-x-(--active-tab-left) bg-foreground transition-[translate,width] duration-(--duration-fast) ease-(--ease-standard)"
            />
        </TabsPrimitive.List>
    );
}

export function TabsTrigger({ className, ...props }: TabsPrimitive.Tab.Props) {
    return (
        <TabsPrimitive.Tab
            data-slot="tabs-trigger"
            className={cn(
                "-mb-px inline-flex shrink-0 items-center gap-1.5 rounded-sm px-0.5 text-sm whitespace-nowrap text-muted-foreground transition-colors duration-(--duration-instant) outline-none hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50 disabled:pointer-events-none disabled:opacity-50 data-active:text-foreground [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-4",
                className,
            )}
            {...props}
        />
    );
}

export function TabsContent({
    className,
    ...props
}: TabsPrimitive.Panel.Props) {
    return (
        <TabsPrimitive.Panel
            data-slot="tabs-content"
            className={cn("flex-1 text-sm outline-none", className)}
            {...props}
        />
    );
}
