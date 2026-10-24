import { useState, type ReactNode, type ComponentProps } from "react";
import { ApiFailure } from "../../api/client";
import { ErrorNotice } from "../../components/ErrorNotice";
import { Button } from "../../ui/Button";
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogTitle,
    DialogTrigger,
} from "../../ui/Dialog";

/** Native selection keeps scope and role controls keyboard operable. */
export function Select(props: ComponentProps<"select">) {
    return (
        <select
            className="h-8 w-full rounded-lg border border-input bg-background px-2 text-sm text-foreground focus-visible:outline-2 focus-visible:outline-ring"
            {...props}
        />
    );
}

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

/** Confirmation names the exact action; closing while pending is refused. */
export function Confirm({
    label,
    description,
    pending,
    error,
    onConfirm,
}: {
    label: string;
    description: ReactNode;
    pending: boolean;
    error: unknown;
    onConfirm: () => void;
}) {
    const [open, setOpen] = useState(false);
    return (
        <Dialog
            open={open}
            onOpenChange={(next) => {
                if (!pending) setOpen(next);
            }}
        >
            <DialogTrigger render={<Button variant="outline" />}>
                {label}
            </DialogTrigger>
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
