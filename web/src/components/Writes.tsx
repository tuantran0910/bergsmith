// Shared behavior for non-idempotent writes: an answer that may have taken
// effect is surfaced instead of retried, and a confirmation names the action.

import { useState, type ReactNode } from "react";
import { ApiFailure } from "../api/client";
import { ErrorNotice } from "./ErrorNotice";
import { Button } from "../ui/Button";
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogTitle,
    DialogTrigger,
} from "../ui/Dialog";

/** Non-idempotent writes stop after an uncertain answer; reconcile via the list. */
export function WriteError({ error }: { error: unknown }) {
    return (
        <>
            {error !== null && <ErrorNotice error={error} />}
            {uncertain(error) && (
                <p role="alert">
                    The change may have completed. Reload this page and check
                    the saved state before starting another action.
                </p>
            )}
        </>
    );
}

export function uncertain(error: unknown): boolean {
    return error instanceof ApiFailure && error.outcomeUncertain;
}

interface ConfirmProps {
    label: string;
    description: ReactNode;
    pending: boolean;
    error: unknown;
    onConfirm: () => void;
}

/** Confirmation names the exact action; closing while pending is refused. */
export function Confirm(props: ConfirmProps) {
    const [open, setOpen] = useState(false);
    return (
        <ConfirmDialog {...props} open={open} onOpenChange={setOpen}>
            <DialogTrigger render={<Button variant="outline" />}>
                {props.label}
            </DialogTrigger>
        </ConfirmDialog>
    );
}

/**
 * The same confirmation, opened by its owner rather than its own button,
 * such as from a menu item (AGENTS: a shortcut that writes opens the page's
 * confirmation dialog). `children` is an optional trigger.
 */
export function ConfirmDialog({
    label,
    description,
    pending,
    error,
    onConfirm,
    open,
    onOpenChange,
    children,
}: ConfirmProps & {
    open: boolean;
    onOpenChange: (open: boolean) => void;
    children?: ReactNode;
}) {
    return (
        <Dialog
            open={open}
            onOpenChange={(next) => {
                if (!pending) onOpenChange(next);
            }}
        >
            {children}
            <DialogContent>
                <DialogTitle>{label}</DialogTitle>
                <DialogDescription>{description}</DialogDescription>
                <WriteError error={error} />
                <Button
                    variant="destructive"
                    disabled={pending || uncertain(error)}
                    onClick={onConfirm}
                >
                    {pending ? "Working…" : `Confirm ${label.toLowerCase()}`}
                </Button>
            </DialogContent>
        </Dialog>
    );
}
