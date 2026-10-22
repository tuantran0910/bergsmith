import { ClockAlertIcon } from "lucide-react";
import { useEffect, useState } from "react";

import { loginUrl } from "../api/auth";
import { browser } from "../browser";
import { timestamp } from "../format";
import { readLastOrganization } from "../organization";
import { Button } from "../ui/Button";

/** How long before the session ends the notice appears. */
export const EXPIRY_WARNING_MS = 5 * 60 * 1000;

// `setTimeout` overflows above 2^31 - 1 ms; a longer wait is taken in steps.
const MAX_TIMER_MS = 2_147_483_647;

/**
 * Whether the session ending at `expiresAt` is within the warning window,
 * re-rendering when it enters it. An unparsable time never warns.
 */
export function useExpiryWarning(expiresAt: string): boolean {
    const warnAt = Date.parse(expiresAt) - EXPIRY_WARNING_MS;
    const [now, setNow] = useState(Date.now);

    useEffect(() => {
        if (Number.isNaN(warnAt) || now >= warnAt) {
            return undefined;
        }
        const wait = Math.max(0, warnAt - Date.now());
        const timer = window.setTimeout(
            () => {
                setNow(Date.now());
            },
            Math.min(wait, MAX_TIMER_MS),
        );
        return () => {
            window.clearTimeout(timer);
        };
    }, [now, warnAt]);

    return now >= warnAt;
}

/**
 * Warns before the session ends and offers sign-in that returns to the same
 * page. Without a remembered organization it can only explain what to do.
 */
export function SessionExpiry({
    expiresAt,
    returnTo,
}: {
    expiresAt: string;
    returnTo: string;
}) {
    const warn = useExpiryWarning(expiresAt);
    if (!warn) {
        return null;
    }
    const organization = readLastOrganization();

    return (
        <div
            role="status"
            className="flex flex-wrap items-center gap-3 border-b border-l-4 border-l-warning bg-card px-4 py-2 text-sm"
        >
            <ClockAlertIcon
                aria-hidden="true"
                className="size-4 text-warning"
            />
            <p className="m-0 flex-1">
                Your session ends at {timestamp(expiresAt)}.{" "}
                {organization === null &&
                    "Sign out and sign in again to keep working."}
            </p>
            {organization !== null && (
                <Button
                    size="sm"
                    onClick={() => {
                        browser.assign(loginUrl(organization, returnTo));
                    }}
                >
                    Sign in again
                </Button>
            )}
        </div>
    );
}
