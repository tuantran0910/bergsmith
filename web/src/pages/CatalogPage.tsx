import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { canManageCatalog } from "../api/auth";
import { useCatalog } from "../api/catalogs";
import { api, call, csrfHeader } from "../api/client";
import { ErrorNotice } from "../components/ErrorNotice";
import { LoadingDetail, PageHeader } from "../components/Page";
import { ResourceId } from "../components/ResourceId";
import { duration, words } from "../format";
import { useSession, useSessionMutation } from "../queryClient";
import { Badge } from "../ui/Badge";
import { Button } from "../ui/Button";
import { Card, CardContent, CardHeader, CardTitle } from "../ui/Card";
import { Capabilities } from "./catalogs/Capabilities";
import { Discovery } from "./catalogs/Discovery";
import { RegisteredTables } from "./catalogs/RegisteredTables";
import { Settings } from "./catalogs/Settings";
import { Access } from "./access/Access";

/** Connection configuration, diagnostics, and its registered names. */
export function CatalogPage({ id }: { id: string }) {
    const client = useQueryClient();
    const query = useCatalog(id);
    const session = useSession();
    const manager = session !== null && canManageCatalog(session, id);
    const [discover, setDiscover] = useState(false);
    const check = useSessionMutation({
        mutationFn: () =>
            call(
                api.POST("/api/v1/catalogs/{id}/check", {
                    params: { path: { id }, header: csrfHeader() },
                }),
            ),
        onSuccess: () => {
            void client.invalidateQueries({
                queryKey: ["catalog", id],
                exact: true,
            });
        },
    });
    if (query.isPending) return <LoadingDetail label="Loading catalog…" />;
    if (query.error)
        return (
            <ErrorNotice
                error={query.error}
                onRetry={() => {
                    void query.refetch();
                }}
            />
        );
    const catalog = query.data;
    return (
        <section className="grid gap-6">
            <PageHeader
                title={catalog.name}
                description={
                    <div className="flex gap-2">
                        <Badge>
                            {catalog.enabled ? "Enabled" : "Disabled"}
                        </Badge>
                        <Badge>
                            {catalog.settings.maintenance_enabled
                                ? "Maintenance on"
                                : "Maintenance paused"}
                        </Badge>
                    </div>
                }
                actions={<Link to="/catalogs">All catalogs</Link>}
            />
            <Card>
                <CardHeader>
                    <CardTitle>Connection details</CardTitle>
                </CardHeader>
                <CardContent className="grid gap-3 text-sm">
                    <dl className="m-0 grid gap-4 sm:grid-cols-2">
                        {catalog.uri && (
                            <div className="sm:col-span-2">
                                <dt className="text-xs text-muted-foreground">
                                    Catalog endpoint
                                </dt>
                                <dd className="m-0 mt-1 break-all font-medium">
                                    {catalog.uri}
                                </dd>
                            </div>
                        )}
                        {catalog.warehouse && (
                            <div>
                                <dt className="text-xs text-muted-foreground">
                                    Warehouse
                                </dt>
                                <dd className="m-0 mt-1 font-medium wrap-anywhere">
                                    {catalog.warehouse}
                                </dd>
                            </div>
                        )}
                        {catalog.credential_ref && (
                            <div>
                                <dt className="text-xs text-muted-foreground">
                                    Credential reference
                                </dt>
                                <dd className="m-0 mt-1 wrap-anywhere">
                                    {catalog.credential_ref}
                                </dd>
                            </div>
                        )}
                        <div>
                            <dt className="text-xs text-muted-foreground">
                                Connection ID
                            </dt>
                            <dd className="m-0 mt-1">
                                <ResourceId
                                    id={catalog.id}
                                    label="connection ID"
                                    short
                                />
                            </dd>
                        </div>
                        {!manager && (
                            <>
                                <div>
                                    <dt className="text-xs text-muted-foreground">
                                        Execution budget
                                    </dt>
                                    <dd className="m-0 mt-1">
                                        {catalog.settings.max_execution_seconds}{" "}
                                        s
                                    </dd>
                                </div>
                                <div>
                                    <dt className="text-xs text-muted-foreground">
                                        Unresolved publication bound
                                    </dt>
                                    <dd className="m-0 mt-1">
                                        {
                                            catalog.settings
                                                .max_unresolved_publication_seconds
                                        }{" "}
                                        s
                                    </dd>
                                </div>
                            </>
                        )}
                    </dl>
                </CardContent>
            </Card>
            {manager && <Settings key={catalog.revision} catalog={catalog} />}
            <div
                className={
                    manager
                        ? "grid items-start gap-6 lg:grid-cols-2"
                        : "grid gap-6"
                }
            >
                {manager && (
                    <div className="grid gap-3">
                        <Capabilities
                            capabilities={catalog.capabilities}
                            checkedAt={catalog.capabilities_checked_at}
                            action={
                                <Button
                                    variant="outline"
                                    disabled={check.isPending}
                                    onClick={() => {
                                        check.mutate();
                                    }}
                                >
                                    {check.isPending
                                        ? "Checking…"
                                        : "Check capabilities"}
                                </Button>
                            }
                        />
                        {check.error !== null && (
                            <ErrorNotice error={check.error} />
                        )}
                    </div>
                )}
                <div className="grid gap-6">
                    {catalog.storage_profile && (
                        <Card>
                            <CardHeader>
                                <CardTitle>Effective storage profile</CardTitle>
                            </CardHeader>
                            <CardContent>
                                <p className="mt-0 mb-4 text-muted-foreground">
                                    {catalog.storage_profile.source ===
                                    "connection"
                                        ? "Connection-specific profiles"
                                        : "Default profiles"}{" "}
                                    · Read-only; managed in coordinator
                                    configuration.
                                </p>
                                {catalog.storage_profile.backends.length ===
                                    0 && <p>No storage backends configured.</p>}
                                <ul className="m-0 grid list-none gap-3 p-0">
                                    {catalog.storage_profile.backends.map(
                                        (backend, index) => (
                                            <li
                                                key={`${backend.kind}-${index}`}
                                                className="grid gap-3 rounded-lg border p-3"
                                            >
                                                <strong className="text-xs tracking-wide uppercase">
                                                    {backend.kind}
                                                </strong>
                                                <dl className="m-0 grid gap-3 sm:grid-cols-2">
                                                    {Object.entries(backend)
                                                        .filter(
                                                            ([key, value]) =>
                                                                key !==
                                                                    "kind" &&
                                                                value !== null,
                                                        )
                                                        .map(([key, value]) => (
                                                            <div
                                                                key={key}
                                                                className={
                                                                    key ===
                                                                    "endpoint"
                                                                        ? "sm:col-span-2"
                                                                        : undefined
                                                                }
                                                            >
                                                                <dt className="text-xs text-muted-foreground">
                                                                    {words(key)}
                                                                </dt>
                                                                <dd className="m-0 mt-1 wrap-anywhere">
                                                                    {typeof value ===
                                                                    "boolean"
                                                                        ? value
                                                                            ? "Enabled"
                                                                            : "Disabled"
                                                                        : String(
                                                                              value,
                                                                          )}
                                                                </dd>
                                                            </div>
                                                        ))}
                                                </dl>
                                            </li>
                                        ),
                                    )}
                                </ul>
                            </CardContent>
                        </Card>
                    )}
                    {catalog.unresolved_publications && (
                        <Card>
                            <CardHeader>
                                <CardTitle>Unresolved publications</CardTitle>
                            </CardHeader>
                            <CardContent>
                                <dl className="m-0 grid grid-cols-2 gap-4">
                                    <div>
                                        <dt className="text-xs text-muted-foreground">
                                            Awaiting resolution
                                        </dt>
                                        <dd className="m-0 mt-1 text-2xl font-semibold tabular-nums">
                                            {
                                                catalog.unresolved_publications
                                                    .count
                                            }
                                        </dd>
                                    </div>
                                    <div>
                                        <dt className="text-xs text-muted-foreground">
                                            Oldest age
                                        </dt>
                                        <dd className="m-0 mt-1 text-lg font-medium tabular-nums">
                                            {catalog.unresolved_publications
                                                .count === 0
                                                ? "None"
                                                : duration(
                                                      catalog
                                                          .unresolved_publications
                                                          .oldest_seconds *
                                                          1000,
                                                  )}
                                        </dd>
                                    </div>
                                </dl>
                                {catalog.unresolved_publications.overdue && (
                                    <p role="alert">
                                        The oldest publication exceeds this
                                        connection’s age bound. Investigate its
                                        outcome; the bound never declares it
                                        failed.
                                    </p>
                                )}
                            </CardContent>
                        </Card>
                    )}
                </div>
            </div>
            <Access
                scope={{ catalog_connection: id }}
                connections={[id]}
                scopeLabel={catalog.name}
            />
            <RegisteredTables id={id} manager={manager} />
            {manager && (
                <div className="grid gap-4">
                    <Button
                        variant="outline"
                        className="w-fit"
                        aria-expanded={discover}
                        onClick={() => {
                            setDiscover(!discover);
                        }}
                    >
                        {discover ? "Hide discovery" : "Discover tables"}
                    </Button>
                    {discover && <Discovery key={catalog.revision} id={id} />}
                </div>
            )}
        </section>
    );
}
