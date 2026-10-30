// The top bar's search button and its shortcut (ADR 0067). The palette's
// code loads on hover, focus, or first use, like `MenuButton`, so the shell's
// first load does not carry it.

import { SearchIcon } from "lucide-react";
import { Suspense, lazy, useEffect, useState } from "react";

import type { SessionInfo } from "../api/auth";
import { Button } from "../ui/Button";
import { Kbd } from "../ui/Kbd";

const loadPalette = () => import("./CommandPalette");
const CommandPalette = lazy(loadPalette);

function preload() {
    void loadPalette();
}

/** Whether `event` opens the palette: Ctrl+K or ⌘K, without Alt or Shift. */
export function isPaletteShortcut(event: KeyboardEvent): boolean {
    return (
        event.key.toLowerCase() === "k" &&
        (event.ctrlKey || event.metaKey) &&
        !event.altKey &&
        !event.shiftKey
    );
}

/** Whether the shortcut reads as ⌘ on this device. */
function usesCommandKey(): boolean {
    return /Mac|iPhone|iPad/.test(navigator.userAgent);
}

/**
 * Opens the command palette: pages, catalogs, tables, and people by name.
 * The palette only navigates; every write stays behind its page's own
 * confirmation.
 */
export function CommandPaletteButton({ session }: { session: SessionInfo }) {
    const [open, setOpen] = useState(false);
    // Mounted from the first opening on, so closing can animate.
    const [mounted, setMounted] = useState(false);
    const command = usesCommandKey();

    useEffect(() => {
        const onKeyDown = (event: KeyboardEvent) => {
            if (event.defaultPrevented || !isPaletteShortcut(event)) {
                return;
            }
            event.preventDefault();
            setMounted(true);
            setOpen((current) => !current);
        };
        window.addEventListener("keydown", onKeyDown);
        return () => {
            window.removeEventListener("keydown", onKeyDown);
        };
    }, []);

    return (
        <>
            <Button
                variant="outline"
                size="sm"
                aria-haspopup="dialog"
                aria-expanded={open}
                aria-keyshortcuts="Control+K Meta+K"
                onPointerEnter={preload}
                onFocus={preload}
                onClick={() => {
                    setMounted(true);
                    setOpen(true);
                }}
                className="text-muted-foreground"
            >
                <SearchIcon aria-hidden="true" />
                Search
                <Kbd aria-hidden="true" className="hidden sm:inline-flex">
                    {command ? "⌘K" : "Ctrl K"}
                </Kbd>
            </Button>
            {mounted && (
                <Suspense fallback={null}>
                    <CommandPalette
                        open={open}
                        onOpenChange={setOpen}
                        session={session}
                    />
                </Suspense>
            )}
        </>
    );
}
