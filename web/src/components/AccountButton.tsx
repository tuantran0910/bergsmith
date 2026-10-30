// The account control at the foot of the rail. Like `MenuButton`, the button
// is plain and eager and the popover loads on hover, focus, or the first
// press, so the floating primitive stays out of the first-load bundle
// (ADR 0067).

import { Suspense, lazy, useId, useRef, useState } from "react";

import { cn } from "../ui/cn";
import type { AccountPanelProps } from "./AccountPanel";

const loadPanel = () => import("./AccountPanel");
const AccountPanel = lazy(loadPanel);

function preload() {
    void loadPanel();
}

/** The first letter of the name, for the avatar; "?" for a blank name. */
export function initialOf(name: string): string {
    const first = name.trim().codePointAt(0);
    return first === undefined
        ? "?"
        : String.fromCodePoint(first).toLocaleUpperCase();
}

/**
 * The signed-in principal's name, opening a panel with the theme, the table
 * density, and sign-out. In the collapsed rail only the avatar shows; the
 * name stays the button's accessible name and its tooltip.
 */
export function AccountButton({
    collapsed,
    className,
    ...panel
}: Omit<AccountPanelProps, "id" | "anchor" | "side" | "onOpenChange"> & {
    collapsed: boolean;
    className?: string;
}) {
    const [open, setOpen] = useState(false);
    const anchor = useRef<HTMLButtonElement>(null);
    const panelId = useId();
    return (
        <>
            <button
                ref={anchor}
                type="button"
                aria-haspopup="dialog"
                aria-expanded={open}
                aria-controls={open ? panelId : undefined}
                title={collapsed ? panel.displayName : undefined}
                onPointerEnter={preload}
                onFocus={preload}
                onClick={() => {
                    setOpen((current) => !current);
                }}
                className={cn(
                    "flex min-w-0 items-center gap-2 rounded-md px-1.5 py-1.5 text-left text-sm text-foreground outline-none hover:bg-muted focus-visible:ring-3 focus-visible:ring-ring/50 aria-expanded:bg-muted",
                    className,
                )}
            >
                <span
                    aria-hidden="true"
                    className="flex size-6 shrink-0 items-center justify-center rounded-full bg-secondary text-xs font-semibold text-secondary-foreground"
                >
                    {initialOf(panel.displayName)}
                </span>
                <span className="truncate rail-collapsed:sr-only">
                    {panel.displayName}
                </span>
            </button>
            {open && (
                <Suspense fallback={null}>
                    <AccountPanel
                        id={panelId}
                        anchor={anchor}
                        side={collapsed ? "right" : "top"}
                        onOpenChange={(next, pressedButton) => {
                            // A press on the button itself closes through its
                            // own click, not the outside-press dismissal.
                            if (!next && pressedButton) {
                                return;
                            }
                            setOpen(next);
                        }}
                        {...panel}
                    />
                </Suspense>
            )}
        </>
    );
}
