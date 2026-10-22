// Adapted from shadcn/ui (base-nova style, MIT). Toasts are announced in a
// polite live region; anything that needs a decision belongs in the page.

import { Toast as ToastPrimitive } from "@base-ui/react/toast";
import {
    CircleCheckIcon,
    InfoIcon,
    OctagonXIcon,
    TriangleAlertIcon,
    XIcon,
} from "lucide-react";

import { Button } from "./Button";

/** Creates the manager that `Toaster` renders and code adds toasts to. */
export const createToastManager = ToastPrimitive.createToastManager;

/** The toast manager of the nearest `Toaster`. */
export const useToastManager = ToastPrimitive.useToastManager;

/** The kinds of toast, each with its own icon. */
export type ToastType = "success" | "info" | "warning" | "error";

const ICONS = {
    success: CircleCheckIcon,
    info: InfoIcon,
    warning: TriangleAlertIcon,
    error: OctagonXIcon,
} as const;

function isToastType(type: string | undefined): type is ToastType {
    return type !== undefined && type in ICONS;
}

function ToastIcon({ type }: { type: string | undefined }) {
    if (!isToastType(type)) {
        return null;
    }
    const Icon = ICONS[type];
    return (
        <Icon
            aria-hidden="true"
            className={
                type === "error"
                    ? "size-4 shrink-0 text-destructive"
                    : "size-4 shrink-0"
            }
        />
    );
}

function ToastList() {
    const { toasts } = ToastPrimitive.useToastManager();
    return toasts.map((item) => (
        <ToastPrimitive.Root
            key={item.id}
            toast={item}
            data-slot="toast"
            className="pointer-events-auto relative w-full rounded-xl border bg-popover text-popover-foreground shadow-lg transition-[opacity,translate] duration-200 outline-none focus-visible:ring-3 focus-visible:ring-ring/50 data-ending-style:translate-y-2 data-ending-style:opacity-0 data-starting-style:translate-y-2 data-starting-style:opacity-0 motion-reduce:transition-none"
        >
            <ToastPrimitive.Content className="flex items-start gap-3 p-4">
                <ToastIcon type={item.type} />
                <div className="flex min-w-0 flex-1 flex-col gap-1">
                    <ToastPrimitive.Title className="text-sm font-medium" />
                    <ToastPrimitive.Description className="text-sm text-muted-foreground" />
                </div>
                <ToastPrimitive.Close
                    aria-label="Dismiss notification"
                    render={<Button variant="ghost" size="icon-sm" />}
                >
                    <XIcon aria-hidden="true" />
                </ToastPrimitive.Close>
            </ToastPrimitive.Content>
        </ToastPrimitive.Root>
    ));
}

/** Provides a toast manager to its children and shows its toasts. */
export function Toaster({ children, ...props }: ToastPrimitive.Provider.Props) {
    return (
        <ToastPrimitive.Provider {...props}>
            {children}
            <ToastPrimitive.Portal>
                <ToastPrimitive.Viewport className="pointer-events-none fixed inset-x-4 bottom-4 z-50 mx-auto flex max-w-sm flex-col gap-2 outline-none sm:right-4 sm:left-auto sm:mx-0 sm:w-full">
                    <ToastList />
                </ToastPrimitive.Viewport>
            </ToastPrimitive.Portal>
        </ToastPrimitive.Provider>
    );
}
