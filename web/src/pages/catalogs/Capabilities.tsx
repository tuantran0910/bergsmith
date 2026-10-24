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
                            <div className="grid gap-1 rounded-lg border px-3 py-2">
                                <dt>Idempotency lifetime (optional)</dt>
                                <dd className="m-0 text-muted-foreground">
                                    {capabilities.idempotency_key_lifetime_seconds ===
                                    null
                                        ? "Not advertised"
                                        : `${capabilities.idempotency_key_lifetime_seconds} s`}
                                </dd>
                            </div>
                        </dl>
                        {checkedAt && (
                            <p className="text-muted-foreground">
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
                <p className="mb-0 text-muted-foreground">
                    The catalog advertises these endpoints. Vended credentials
                    are only proven by loading a table; unknown does not mean
                    supported.
                </p>
            </CardContent>
        </Card>
    );
}
