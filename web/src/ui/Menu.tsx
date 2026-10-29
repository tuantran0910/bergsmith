// Adapted from shadcn/ui (base-nova style, MIT). Base UI positions the popup
// through CSSOM properties, which the CSP allows (ADR 0067).

import { Menu as MenuPrimitive } from "@base-ui/react/menu";
import { CheckIcon } from "lucide-react";
import type { ComponentProps } from "react";

import { cn } from "./cn";

/** A menu of actions or choices opened from a trigger; arrows move, Escape closes. */
export function Menu(props: MenuPrimitive.Root.Props) {
    return <MenuPrimitive.Root {...props} />;
}

export function MenuTrigger(props: MenuPrimitive.Trigger.Props) {
    return <MenuPrimitive.Trigger data-slot="menu-trigger" {...props} />;
}

/** The floating list, flipped and shifted to stay on screen. */
export function MenuContent({
    className,
    side = "bottom",
    align = "start",
    sideOffset = 4,
    anchor,
    ...props
}: MenuPrimitive.Popup.Props &
    Pick<
        MenuPrimitive.Positioner.Props,
        "align" | "side" | "sideOffset" | "anchor"
    >) {
    return (
        <MenuPrimitive.Portal>
            <MenuPrimitive.Positioner
                side={side}
                align={align}
                sideOffset={sideOffset}
                anchor={anchor}
                className="isolate z-50 outline-none"
            >
                <MenuPrimitive.Popup
                    data-slot="menu-content"
                    className={cn(
                        "max-h-(--available-height) min-w-40 origin-(--transform-origin) overflow-y-auto rounded-md border bg-popover p-1 text-sm text-popover-foreground shadow-md transition-[opacity,scale] duration-(--duration-fast) ease-(--ease-out) outline-none data-ending-style:scale-[0.97] data-ending-style:opacity-0 data-ending-style:ease-(--ease-in) data-starting-style:scale-[0.97] data-starting-style:opacity-0",
                        className,
                    )}
                    {...props}
                />
            </MenuPrimitive.Positioner>
        </MenuPrimitive.Portal>
    );
}

const itemClass =
    "relative flex min-h-7 cursor-default items-center gap-2 rounded-sm px-2 py-1 outline-none select-none data-disabled:pointer-events-none data-disabled:opacity-50 data-highlighted:bg-accent data-highlighted:text-accent-foreground [&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-4";

export function MenuItem({
    className,
    variant = "default",
    ...props
}: MenuPrimitive.Item.Props & {
    variant?: "default" | "destructive" | undefined;
}) {
    return (
        <MenuPrimitive.Item
            data-slot="menu-item"
            data-variant={variant}
            className={cn(
                itemClass,
                "data-[variant=destructive]:text-destructive",
                className,
            )}
            {...props}
        />
    );
}

/** A menu item that navigates; render a router link through `render`. */
export function MenuLinkItem({
    className,
    ...props
}: MenuPrimitive.LinkItem.Props) {
    return (
        <MenuPrimitive.LinkItem
            data-slot="menu-link-item"
            className={cn(
                itemClass,
                "no-underline hover:no-underline",
                className,
            )}
            {...props}
        />
    );
}

export function MenuGroup(props: MenuPrimitive.Group.Props) {
    return <MenuPrimitive.Group data-slot="menu-group" {...props} />;
}

export function MenuGroupLabel({
    className,
    ...props
}: MenuPrimitive.GroupLabel.Props) {
    return (
        <MenuPrimitive.GroupLabel
            data-slot="menu-group-label"
            className={cn(
                "px-2 pt-1.5 pb-1 text-2xs font-semibold text-muted-foreground",
                className,
            )}
            {...props}
        />
    );
}

export function MenuSeparator({
    className,
    ...props
}: MenuPrimitive.Separator.Props) {
    return (
        <MenuPrimitive.Separator
            data-slot="menu-separator"
            className={cn("-mx-1 my-1 h-px bg-border", className)}
            {...props}
        />
    );
}

/** One choice among several; the checked item shows a check. */
export function MenuRadioGroup(props: MenuPrimitive.RadioGroup.Props) {
    return <MenuPrimitive.RadioGroup data-slot="menu-radio-group" {...props} />;
}

export function MenuRadioItem({
    className,
    children,
    ...props
}: MenuPrimitive.RadioItem.Props) {
    return (
        <MenuPrimitive.RadioItem
            data-slot="menu-radio-item"
            className={cn(itemClass, "pr-8", className)}
            {...props}
        >
            {children}
            <MenuPrimitive.RadioItemIndicator className="absolute right-2 flex items-center">
                <CheckIcon aria-hidden="true" />
            </MenuPrimitive.RadioItemIndicator>
        </MenuPrimitive.RadioItem>
    );
}

/** A shortcut hint aligned to the end of an item. */
export function MenuShortcut({ className, ...props }: ComponentProps<"span">) {
    return (
        <span
            data-slot="menu-shortcut"
            className={cn(
                "ml-auto text-2xs tracking-wide text-muted-foreground",
                className,
            )}
            {...props}
        />
    );
}
