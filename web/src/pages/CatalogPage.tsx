import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { useState } from "react";
import { canManageCatalog, type SessionInfo } from "../api/auth";
import { useCatalog } from "../api/catalogs";
import { api, call, csrfHeader } from "../api/client";
import { ErrorNotice } from "../components/ErrorNotice";
import { LoadingDetail, PageHeader } from "../components/Page";
import { ResourceId } from "../components/ResourceId";
import { SESSION_KEY, useSessionMutation } from "../queryClient";
import { Badge } from "../ui/Badge";
import { Button } from "../ui/Button";
import { Card, CardContent, CardHeader, CardTitle } from "../ui/Card";
import { Capabilities } from "./catalogs/Capabilities";
import { Discovery } from "./catalogs/Discovery";
import { RegisteredTables } from "./catalogs/RegisteredTables";
import { Settings } from "./catalogs/Settings";

/** Connection configuration, diagnostics, and its registered names. */
export function CatalogPage({ id }: { id: string }) {
    const client = useQueryClient();
    const query = useCatalog(id);
    const session = client.getQueryData<SessionInfo>(SESSION_KEY);
    const manager = !!session && canManageCatalog(session, id);
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
                <CardContent className="grid gap-3 text-sm">
                    <ResourceId id={catalog.id} label="connection ID" />
                    {catalog.uri && (
                        <p className="m-0 break-all">{catalog.uri}</p>
                    )}
                    {catalog.warehouse && (
                        <p className="m-0">Warehouse: {catalog.warehouse}</p>
                    )}
                    {catalog.credential_ref && (
                        <p className="m-0">
                            Credential reference: {catalog.credential_ref}
                        </p>
                    )}
                    <p className="m-0">
                        Execution budget:{" "}
                        {catalog.settings.max_execution_seconds} s · Unresolved
                        publication bound:{" "}
                        {catalog.settings.max_unresolved_publication_seconds} s
                    </p>
                </CardContent>
            </Card>
            {manager && (
                <>
                    <div>
                        <Button
                            disabled={check.isPending}
                            onClick={() => {
                                check.mutate();
                            }}
                        >
                            {check.isPending
                                ? "Checking…"
                                : "Check capabilities"}
                        </Button>
                        {check.error !== null && (
                            <ErrorNotice error={check.error} />
                        )}
                    </div>
                    <Capabilities
                        capabilities={catalog.capabilities}
                        checkedAt={catalog.capabilities_checked_at}
                    />
                    <Settings key={catalog.revision} catalog={catalog} />
                </>
            )}
            {catalog.storage_profile && (
                <Card>
                    <CardHeader>
                        <CardTitle>Effective storage profile</CardTitle>
                    </CardHeader>
                    <CardContent>
                        <p className="mt-0">
                            {catalog.storage_profile.source === "connection"
                                ? "Connection-specific profiles"
                                : "Default profiles"}{" "}
                            · Read-only; managed in coordinator configuration.
                        </p>
                        {catalog.storage_profile.backends.length === 0 && (
                            <p>No storage backends configured.</p>
                        )}
                        <ul className="grid gap-2">
                            {catalog.storage_profile.backends.map(
                                (backend, index) => (
                                    <li key={`${backend.kind}-${index}`}>
                                        <strong>{backend.kind}</strong>
                                        {Object.entries(backend)
                                            .filter(
                                                ([key, value]) =>
                                                    key !== "kind" &&
                                                    value !== null,
                                            )
                                            .map(([key, value]) => (
                                                <span
                                                    key={key}
                                                    className="ml-2 break-all"
                                                >
                                                    {key.replaceAll("_", " ")}:{" "}
                                                    {String(value)}
                                                </span>
                                            ))}
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
                        <p className="mt-0">
                            {catalog.unresolved_publications.count} unresolved ·
                            Oldest age:{" "}
                            {catalog.unresolved_publications.oldest_seconds} s
                        </p>
                        {catalog.unresolved_publications.overdue && (
                            <p role="alert">
                                The oldest publication exceeds this connection’s
                                age bound. Investigate its outcome; the bound
                                never declares it failed.
                            </p>
                        )}
                    </CardContent>
                </Card>
            )}
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
