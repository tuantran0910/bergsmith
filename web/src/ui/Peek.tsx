// A side panel that previews a list row without leaving the list (ADR 0067).

import { Dialog as DialogPrimitive } from "@base-ui/react/dialog";
import { XIcon } from "lucide-react";
import type { ComponentProps, ReactNode } from "react";

import { Button } from "./Button";
import { cn } from "./cn";

/**
 * The panel's open state. It is modal, like the menus: a non-modal Base UI
 * popup adds focusable, `aria-hidden` focus guards that fail WCAG checks.
 * The page stays visible beside it without a backdrop; a press outside or
 * Escape closes it, and focus returns to the row that opened it. Keep the
 * open item in the address (`?peek=`) so a reload or a shared link reopens
 * it.
 */
export function Peek(props: DialogPrimitive.Root.Props) {
    return <DialogPrimitive.Root {...props} />;
}

/** The panel: a full-height sheet from the end edge, full width on phones. */
export function PeekContent({
    className,
    children,
    title,
    actions,
    ...props
}: Omit<DialogPrimitive.Popup.Props, "title"> & {
    /** The previewed item's name, shown as the panel heading. */
    title: ReactNode;
    /** Controls beside the close button, such as "Open". */
    actions?: ReactNode;
}) {
    return (
        <DialogPrimitive.Portal>
            <DialogPrimitive.Popup
                data-slot="peek-content"
                className={cn(
                    "fixed inset-y-0 right-0 z-40 flex w-full flex-col border-l bg-surface-raised text-sm text-foreground shadow-xl transition-[translate,opacity] duration-(--duration-base) ease-(--ease-out) outline-none data-ending-style:translate-x-4 data-ending-style:opacity-0 data-ending-style:duration-(--duration-fast) data-ending-style:ease-(--ease-in) data-starting-style:translate-x-4 data-starting-style:opacity-0 sm:w-(--peek-width)",
                    className,
                )}
                {...props}
            >
                <header className="flex h-(--header-height) shrink-0 items-center gap-2 border-b px-4">
                    <DialogPrimitive.Title className="m-0 min-w-0 flex-1 truncate text-base font-semibold">
                        {title}
                    </DialogPrimitive.Title>
                    {actions}
                    <DialogPrimitive.Close
                        render={<Button variant="ghost" size="icon-sm" />}
                    >
                        <XIcon aria-hidden="true" />
                        <span className="sr-only">Close preview</span>
                    </DialogPrimitive.Close>
                </header>
                <div className="min-h-0 flex-1 overflow-y-auto p-4">
                    {children}
                </div>
            </DialogPrimitive.Popup>
        </DialogPrimitive.Portal>
    );
}

/** A labelled group of facts inside the panel. */
export function PeekSection({
    className,
    title,
    children,
    ...props
}: Omit<ComponentProps<"section">, "title"> & { title: string }) {
    return (
        <section
            data-slot="peek-section"
            aria-label={title}
            className={cn(
                "border-b py-3 first:pt-0 last:border-b-0",
                className,
            )}
            {...props}
        >
            <h3 className="m-0 mb-2 text-xs font-semibold text-muted-foreground">
                {title}
            </h3>
            {children}
        </section>
    );
}
