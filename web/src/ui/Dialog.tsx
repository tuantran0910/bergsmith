// Adapted from shadcn/ui (base-nova style, MIT).

import { Dialog as DialogPrimitive } from "@base-ui/react/dialog";
import { XIcon } from "lucide-react";
import type { ComponentProps } from "react";

import { Button } from "./Button";
import { cn } from "./cn";

/** A modal dialog. Focus is trapped while open and returns to the trigger. */
export function Dialog(props: DialogPrimitive.Root.Props) {
    return <DialogPrimitive.Root {...props} />;
}

export function DialogTrigger(props: DialogPrimitive.Trigger.Props) {
    return <DialogPrimitive.Trigger data-slot="dialog-trigger" {...props} />;
}

export function DialogClose(props: DialogPrimitive.Close.Props) {
    return <DialogPrimitive.Close data-slot="dialog-close" {...props} />;
}

/** A viewport-bounded, scrollable panel whose content wraps long values. */
export function DialogContent({
    className,
    children,
    showCloseButton = true,
    ...props
}: DialogPrimitive.Popup.Props & { showCloseButton?: boolean }) {
    return (
        <DialogPrimitive.Portal>
            <DialogPrimitive.Backdrop
                data-slot="dialog-overlay"
                className="fixed inset-0 z-50 bg-black/30 transition-opacity duration-(--duration-base) ease-(--ease-out) data-ending-style:duration-(--duration-fast) data-ending-style:ease-(--ease-in) data-ending-style:opacity-0 data-starting-style:opacity-0"
            />
            <DialogPrimitive.Popup
                data-slot="dialog-content"
                className={cn(
                    "fixed top-1/2 left-1/2 z-50 grid max-h-[calc(100dvh-2rem)] w-full max-w-[calc(100%-2rem)] -translate-x-1/2 -translate-y-1/2 grid-cols-[minmax(0,1fr)] gap-4 overflow-y-auto rounded-lg border bg-popover p-4 text-sm wrap-anywhere text-popover-foreground shadow-lg transition-[opacity,scale] duration-(--duration-emphasis) ease-(--ease-out) outline-none data-ending-style:scale-[0.98] data-ending-style:opacity-0 data-ending-style:duration-(--duration-fast) data-ending-style:ease-(--ease-in) data-starting-style:scale-[0.96] data-starting-style:opacity-0 sm:max-w-md",
                    className,
                )}
                {...props}
            >
                {children}
                {showCloseButton && (
                    <DialogPrimitive.Close
                        data-slot="dialog-close"
                        render={
                            <Button
                                variant="ghost"
                                size="icon-sm"
                                className="absolute top-2 right-2"
                            />
                        }
                    >
                        <XIcon aria-hidden="true" />
                        <span className="sr-only">Close</span>
                    </DialogPrimitive.Close>
                )}
            </DialogPrimitive.Popup>
        </DialogPrimitive.Portal>
    );
}

export function DialogHeader({ className, ...props }: ComponentProps<"div">) {
    return (
        <div
            data-slot="dialog-header"
            className={cn("flex flex-col gap-2 pr-8", className)}
            {...props}
        />
    );
}

export function DialogFooter({ className, ...props }: ComponentProps<"div">) {
    return (
        <div
            data-slot="dialog-footer"
            className={cn(
                "-mx-4 -mb-4 flex flex-col-reverse gap-2 rounded-b-lg border-t bg-surface-sunken p-3 sm:flex-row sm:justify-end",
                className,
            )}
            {...props}
        />
    );
}

export function DialogTitle({
    className,
    ...props
}: DialogPrimitive.Title.Props) {
    return (
        <DialogPrimitive.Title
            data-slot="dialog-title"
            className={cn("text-base leading-none font-semibold", className)}
            {...props}
        />
    );
}

export function DialogDescription({
    className,
    ...props
}: DialogPrimitive.Description.Props) {
    return (
        <DialogPrimitive.Description
            data-slot="dialog-description"
            className={cn("text-sm text-muted-foreground", className)}
            {...props}
        />
    );
}
