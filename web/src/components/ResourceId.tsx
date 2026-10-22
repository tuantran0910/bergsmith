import { CheckIcon, CopyIcon, XIcon } from "lucide-react";
import { useEffect, useState } from "react";

import { shortId } from "../format";
import { Button } from "../ui/Button";

type CopyState = "idle" | "copied" | "failed";

/** How long the copy button shows its result before resetting. */
export const COPY_FEEDBACK_MS = 2000;

/**
 * Copies `value` to the clipboard and reports the result. The Clipboard API
 * exists only in secure contexts, so over plain HTTP there is no button.
 */
export function CopyButton({ value, label }: { value: string; label: string }) {
    const [state, setState] = useState<CopyState>("idle");

    useEffect(() => {
        if (state === "idle") {
            return undefined;
        }
        const timer = window.setTimeout(() => {
            setState("idle");
        }, COPY_FEEDBACK_MS);
        return () => {
            window.clearTimeout(timer);
        };
    }, [state]);

    if (!("clipboard" in navigator)) {
        return null;
    }
    const Icon =
        state === "copied" ? CheckIcon : state === "failed" ? XIcon : CopyIcon;
    return (
        <>
            <Button
                variant="ghost"
                size="icon-sm"
                className="size-6 text-muted-foreground"
                aria-label={`Copy ${label}`}
                onClick={() => {
                    navigator.clipboard.writeText(value).then(
                        () => {
                            setState("copied");
                        },
                        () => {
                            setState("failed");
                        },
                    );
                }}
            >
                <Icon aria-hidden="true" />
            </Button>
            <span role="status" className="sr-only">
                {state === "copied" && `Copied ${label}`}
                {state === "failed" && `Could not copy ${label}`}
            </span>
        </>
    );
}

/**
 * An ID in monospace with a copy button. `short` shows only its tail, with
 * the full value in the title, for dense lists.
 */
export function ResourceId({
    id,
    label,
    short = false,
}: {
    id: string;
    label: string;
    short?: boolean;
}) {
    return (
        <span className="inline-flex max-w-full items-center gap-1">
            <code title={short ? id : undefined}>
                {short ? shortId(id) : id}
            </code>
            <CopyButton value={id} label={label} />
        </span>
    );
}
