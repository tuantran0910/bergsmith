import type { Schemas } from "../../api/client";
import type { ReactNode } from "react";
import { timestamp } from "../../format";
import { Badge } from "../../ui/Badge";
import {
    Card,
    CardContent,
    CardDescription,
    CardHeader,
    CardTitle,
} from "../../ui/Card";

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
                        <dl className="m-0 grid gap-2">
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
                            ).map(([name, state]) => (
                                <div
                                    key={name}
                                    className="flex items-center justify-between gap-3 rounded-lg border px-3 py-2"
                                >
                                    <dt>{name}</dt>
                                    <dd className="m-0">
                                        <Badge
                                            variant={
                                                state === "met"
                                                    ? "success"
                                                    : state === "missing"
                                                      ? "warning"
                                                      : "neutral"
                                            }
                                        >
                                            {state}
                                        </Badge>
                                    </dd>
                                </div>
                            ))}
                            <div className="flex items-center justify-between gap-3 rounded-lg border px-3 py-2">
                                <dt>Idempotency lifetime (optional)</dt>
                                <dd className="m-0 font-semibold tabular-nums text-muted-foreground">
                                    {capabilities.idempotency_key_lifetime_seconds ===
                                    null
                                        ? "Not advertised"
                                        : `${capabilities.idempotency_key_lifetime_seconds} s`}
                                </dd>
                            </div>
                        </dl>
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
