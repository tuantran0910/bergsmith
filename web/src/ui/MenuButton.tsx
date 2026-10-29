// A menu whose code loads on first use (ADR 0067). The floating menu costs
// about 26 kB gzipped, too much for the first-load bundle of a shell that
// shows a menu button on every page. The button itself is plain and eager;
// the popup loads on hover, focus, or the first press, and then anchors to
// the button.

import type { LucideIcon } from "lucide-react";
import {
    type ReactElement,
    type ReactNode,
    Suspense,
    lazy,
    useId,
    useRef,
    useState,
} from "react";

import { Button, type ButtonProps } from "./Button";

/** One entry of a lazily loaded menu. */
export interface MenuEntry {
    /** Unique within the menu. */
    key: string;
    label: string;
    icon?: LucideIcon;
    /** Renders the item as this element, such as a router `<Link>`. */
    render?: ReactElement;
    onSelect?: () => void;
    variant?: "default" | "destructive";
}

const loadPanel = () => import("./MenuPanel");
const MenuPanel = lazy(loadPanel);

/** Starts loading the menu code, so the first press opens without delay. */
function preload() {
    void loadPanel();
}

/**
 * A button that opens a menu of actions or links. The button is a native
 * `aria-haspopup` control; the menu supplies arrow-key navigation, typeahead,
 * Escape, and focus return to the button.
 */
export function MenuButton({
    children,
    items,
    align = "start",
    variant = "outline",
    size = "sm",
    ...props
}: Omit<ButtonProps, "onClick" | "children"> & {
    children: ReactNode;
    items: readonly MenuEntry[];
    align?: "start" | "center" | "end";
}) {
    const [open, setOpen] = useState(false);
    const anchor = useRef<HTMLButtonElement>(null);
    const menuId = useId();
    return (
        <>
            <Button
                ref={anchor}
                variant={variant}
                size={size}
                aria-haspopup="menu"
                aria-expanded={open}
                aria-controls={open ? menuId : undefined}
                onPointerEnter={preload}
                onFocus={preload}
                onClick={() => {
                    setOpen((current) => !current);
                }}
                {...props}
            >
                {children}
            </Button>
            {open && (
                <Suspense fallback={null}>
                    <MenuPanel
                        id={menuId}
                        anchor={anchor}
                        align={align}
                        items={items}
                        onOpenChange={(next, pressedButton) => {
                            // A press on the button itself closes through its
                            // own click, not the outside-press dismissal.
                            if (!next && pressedButton) {
                                return;
                            }
                            setOpen(next);
                        }}
                    />
                </Suspense>
            )}
        </>
    );
}
