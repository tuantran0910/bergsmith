import type { Schemas } from "../../api/client";
import type { ReactNode } from "react";
import { timestamp } from "../../format";
import {
    CircleCheckIcon,
    CircleDashedIcon,
    CircleAlertIcon,
    TimerIcon,
    type LucideIcon,
} from "lucide-react";
import { cn } from "../../ui/cn";
import {
    Card,
    CardContent,
    CardDescription,
    CardHeader,
    CardTitle,
} from "../../ui/Card";

/**
 * Each check's mark. The state is also written beside it, so the mark never
 * carries the meaning alone.
 */
const MARKS: Record<
    Schemas["CatalogCapabilities"]["list_tables"],
    { icon: LucideIcon; className: string }
> = {
    met: { icon: CircleCheckIcon, className: "text-success" },
    missing: { icon: CircleAlertIcon, className: "text-warning" },
    unknown: { icon: CircleDashedIcon, className: "text-muted-foreground" },
};

/** Advertised support is diagnostic; unknown vending requires a real table load. */
export function Capabilities({
    capabilities,
    checkedAt,
    action,
}: {
    capabilities?: Schemas["CatalogCapabilities"] | undefined;
    checkedAt?: string | undefined;
    action?: ReactNode;
}) {
    return (
        <Card>
            <CardHeader className="flex flex-wrap items-start justify-between gap-3">
                <div className="grid gap-1">
                    <CardTitle>Capabilities</CardTitle>
                    <CardDescription>
                        Support advertised by this catalog.
                    </CardDescription>
                </div>
                {action}
            </CardHeader>
            <CardContent>
                {capabilities ? (
                    <>
                        <ul
                            aria-label="Capability checks"
                            className="m-0 grid list-none divide-y p-0"
                        >
                            {(
                                [
                                    ["List tables", capabilities.list_tables],
                                    ["Load table", capabilities.load_table],
                                    ["Commit table", capabilities.commit_table],
                                    [
                                        "Vended credentials",
                                        capabilities.vended_credentials,
                                    ],
                                    [
                                        "Credentials endpoint (optional)",
                                        capabilities.credentials_endpoint,
                                    ],
                                ] as const
                            ).map(([name, state]) => {
                                const { icon: Icon, className } = MARKS[state];
                                return (
                                    <li
                                        key={name}
                                        className="flex items-center gap-2 py-2 motion-safe:animate-check-in"
                                    >
                                        <Icon
                                            aria-hidden="true"
                                            className={cn(
                                                "size-4 shrink-0",
                                                className,
                                            )}
                                        />
                                        <span className="flex-1">{name}</span>
                                        <span className="text-muted-foreground">
                                            {state}
                                        </span>
                                    </li>
                                );
                            })}
                            <li className="flex items-center gap-2 py-2 motion-safe:animate-check-in">
                                <TimerIcon
                                    aria-hidden="true"
                                    className="size-4 shrink-0 text-muted-foreground"
                                />
                                <span className="flex-1">
                                    Idempotency lifetime (optional)
                                </span>
                                <span className="text-muted-foreground tabular-nums">
                                    {capabilities.idempotency_key_lifetime_seconds ===
                                    null
                                        ? "Not advertised"
                                        : `${capabilities.idempotency_key_lifetime_seconds} s`}
                                </span>
                            </li>
                        </ul>
                        {checkedAt && (
                            <p className="m-0 mt-3 text-xs text-muted-foreground">
                                Checked {timestamp(checkedAt)}
                            </p>
                        )}
                    </>
                ) : (
                    <p>
                        No current capability check. Check again after changing
                        routing settings.
                    </p>
                )}
                <aside className="mt-3 grid gap-1 rounded-lg bg-muted/50 px-3 py-2 text-xs text-muted-foreground">
                    <p className="m-0 font-semibold text-foreground">
                        Reading this report
                    </p>
                    <p className="m-0">
                        Endpoints are advertised by the catalog. Credential
                        vending is verified only when a table loads; unknown
                        means unverified.
                    </p>
                </aside>
            </CardContent>
        </Card>
    );
}
