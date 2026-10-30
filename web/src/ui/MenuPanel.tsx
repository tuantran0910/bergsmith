// The lazily loaded half of `MenuButton`: the Base UI menu, open while
// mounted, anchored to the button that requested it.

import { Menu as MenuPrimitive } from "@base-ui/react/menu";
import type { RefObject } from "react";

import { MenuContent, MenuItem, MenuLinkItem } from "./Menu";
import type { MenuEntry } from "./MenuButton";

export default function MenuPanel({
    id,
    anchor,
    align,
    items,
    onOpenChange,
}: {
    id: string;
    anchor: RefObject<HTMLButtonElement | null>;
    align: "start" | "center" | "end";
    items: readonly MenuEntry[];
    /** `pressedButton` is true when an outside press landed on the anchor. */
    onOpenChange: (open: boolean, pressedButton: boolean) => void;
}) {
    return (
        <MenuPrimitive.Root
            defaultOpen
            onOpenChange={(open, details) => {
                const target = details.event.target;
                const pressedButton =
                    target instanceof Node &&
                    anchor.current?.contains(target) === true;
                onOpenChange(open, pressedButton);
            }}
        >
            <MenuContent
                id={id}
                anchor={anchor}
                align={align}
                finalFocus={anchor}
            >
                {items.map(
                    ({ key, label, icon: Icon, render, onSelect, variant }) => {
                        const content = (
                            <>
                                {Icon !== undefined && (
                                    <Icon aria-hidden="true" />
                                )}
                                {label}
                            </>
                        );
                        return render !== undefined ? (
                            <MenuLinkItem key={key} render={render}>
                                {content}
                            </MenuLinkItem>
                        ) : (
                            <MenuItem
                                key={key}
                                variant={variant}
                                onClick={onSelect}
                            >
                                {content}
                            </MenuItem>
                        );
                    },
                )}
            </MenuContent>
        </MenuPrimitive.Root>
    );
}
