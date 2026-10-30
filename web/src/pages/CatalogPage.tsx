import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import {
    CircleCheckIcon,
    CircleSlashIcon,
    PauseIcon,
    WrenchIcon,
} from "lucide-react";
import { useState } from "react";
import { canManageCatalog } from "../api/auth";
import { useCatalog, useCatalogTotals } from "../api/catalogs";
import { api, call, csrfHeader } from "../api/client";
import { useBreadcrumbLabel } from "../components/Breadcrumbs";
import { ErrorNotice } from "../components/ErrorNotice";
import { Metric, MetricGrid } from "../components/Metric";
import { DetailHeader, LoadingDetail } from "../components/Page";
import { ResourceId } from "../components/ResourceId";
import { TechnicalDetails } from "../components/TechnicalDetails";
import {
    duration,
    exactBytes,
    exactCount,
    relativeTime,
    words,
} from "../format";
import { useSession, useSessionMutation } from "../queryClient";
import { Badge } from "../ui/Badge";
import { Button } from "../ui/Button";
import { Card, CardContent, CardHeader, CardTitle } from "../ui/Card";
import { Skeleton } from "../ui/Skeleton";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../ui/Tabs";
import { Access } from "./access/Access";
import { canManageScope } from "./access/Grants";
import { Capabilities } from "./catalogs/Capabilities";
import { Discovery } from "./catalogs/Discovery";
import { RegisteredTables } from "./catalogs/RegisteredTables";
import {
    catalogTab,
    type CatalogTab,
    type CatalogSearch,
} from "./catalogs/search";
import { Settings } from "./catalogs/Settings";
import { CatalogStorage } from "./catalogs/Storage";

/** Connection configuration, diagnostics, and its registered names. */
export function CatalogPage({ id }: { id: string }) {
    const client = useQueryClient();
    const navigate = useNavigate();
    const search = useSearch({ from: "/catalogs/$catalogId" });
    const current: CatalogTab = catalogTab(search.tab) ?? "overview";
    const query = useCatalog(id);
    const totals = useCatalogTotals(id);
    const session = useSession();
    const manager = session !== null && canManageCatalog(session, id);
    const access =
        session !== null &&
        canManageScope(session, { catalog_connection: id }, [id]);
    const shown: CatalogTab =
        (current === "access" && !access) ||
        (current === "settings" && !manager)
            ? "overview"
            : current;
    useBreadcrumbLabel(query.data?.name);
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
        <Tabs
            value={shown}
            onValueChange={(next) => {
                const selected = catalogTab(next);
                void navigate({
                    to: "/catalogs/$catalogId",
                    params: { catalogId: id },
                    search:
                        selected === undefined || selected === "overview"
                            ? {}
                            : ({ tab: selected } satisfies CatalogSearch),
                    replace: true,
                });
            }}
        >
            <DetailHeader
                title={catalog.name}
                status={
                    <>
                        <Badge
                            variant={catalog.enabled ? "success" : "neutral"}
                        >
                            {catalog.enabled ? (
                                <CircleCheckIcon aria-hidden="true" />
                            ) : (
                                <CircleSlashIcon aria-hidden="true" />
                            )}
                            {catalog.enabled ? "Enabled" : "Disabled"}
                        </Badge>
                        <Badge>
                            {catalog.settings.maintenance_enabled ? (
                                <WrenchIcon aria-hidden="true" />
                            ) : (
                                <PauseIcon aria-hidden="true" />
                            )}
                            {catalog.settings.maintenance_enabled
                                ? "Maintenance on"
                                : "Maintenance paused"}
                        </Badge>
                    </>
                }
                meta={
                    catalog.uri ? (
                        <span className="font-mono break-all">
                            {catalog.uri}
                        </span>
                    ) : undefined
                }
                actions={<Link to="/catalogs">All catalogs</Link>}
                tabs={
                    <TabsList>
                        <TabsTrigger value="overview">Overview</TabsTrigger>
                        <TabsTrigger value="tables">Tables</TabsTrigger>
                        {access && (
                            <TabsTrigger value="access">Access</TabsTrigger>
                        )}
                        {manager && (
                            <TabsTrigger value="settings">Settings</TabsTrigger>
                        )}
                    </TabsList>
                }
            />
            <TabsContent value="overview" className="grid gap-6">
                {manager && (
                    <Card>
                        <CardHeader>
                            <CardTitle>Table discovery</CardTitle>
                        </CardHeader>
                        <CardContent className="grid gap-3">
                            <p className="m-0 text-sm text-muted-foreground">
                                Discovery reads the catalog and registers the
                                tables it can see. It creates no work.
                            </p>
                            <Button
                                variant="outline"
                                className="w-fit"
                                aria-expanded={discover}
                                onClick={() => {
                                    setDiscover(!discover);
                                }}
                            >
                                {discover
                                    ? "Hide discovery"
                                    : "Discover tables"}
                            </Button>
                            {discover && (
                                <Discovery key={catalog.revision} id={id} />
                            )}
                        </CardContent>
                    </Card>
                )}
                <CatalogStorage id={id} />
                <Card>
                    <CardHeader>
                        <CardTitle>Table statistics</CardTitle>
                    </CardHeader>
                    <CardContent>
                        {totals.isPending && (
                            <Skeleton className="h-16 w-full" />
                        )}
                        {totals.error && (
                            <ErrorNotice
                                error={totals.error}
                                onRetry={() => {
                                    void totals.refetch();
                                }}
                            />
                        )}
                        {totals.data && totals.data.tables === "0" && (
                            <p className="m-0 text-sm text-muted-foreground">
                                No sampled tables yet. The coordinator samples
                                every registered table on its interval.
                            </p>
                        )}
                        {totals.data && totals.data.tables !== "0" && (
                            <MetricGrid>
                                <Metric
                                    label="Tables sampled"
                                    value={exactCount(totals.data.tables)}
                                    detail={
                                        totals.data.last_sampled_at === null
                                            ? "Never sampled"
                                            : `Last sampled ${relativeTime(totals.data.last_sampled_at)}`
                                    }
                                />
                                <Metric
                                    label="Rows"
                                    value={exactCount(totals.data.record_count)}
                                />
                                <Metric
                                    label="Data size"
                                    value={exactBytes(totals.data.data_bytes)}
                                />
                                <Metric
                                    label="Data files"
                                    value={exactCount(totals.data.data_files)}
                                    detail={`${exactCount(totals.data.small_files)} small`}
                                />
                                <Metric
                                    label="Delete files"
                                    value={exactCount(totals.data.delete_files)}
                                    detail={exactBytes(
                                        totals.data.delete_bytes,
                                    )}
                                />
                                <Metric
                                    label="Snapshots"
                                    value={exactCount(
                                        totals.data.snapshot_count,
                                    )}
                                />
                            </MetricGrid>
                        )}
                    </CardContent>
                </Card>
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
                                    <dd className="m-0 mt-1 break-all font-semibold">
                                        {catalog.uri}
                                    </dd>
                                </div>
                            )}
                            {catalog.warehouse && (
                                <div>
                                    <dt className="text-xs text-muted-foreground">
                                        Warehouse
                                    </dt>
                                    <dd className="m-0 mt-1 font-semibold wrap-anywhere">
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
                            {!manager && (
                                <>
                                    <div>
                                        <dt className="text-xs text-muted-foreground">
                                            Execution budget
                                        </dt>
                                        <dd className="m-0 mt-1">
                                            {duration(
                                                catalog.settings
                                                    .max_execution_seconds *
                                                    1000,
                                            )}
                                        </dd>
                                    </div>
                                    <div>
                                        <dt className="text-xs text-muted-foreground">
                                            Unresolved publication bound
                                        </dt>
                                        <dd className="m-0 mt-1">
                                            {duration(
                                                catalog.settings
                                                    .max_unresolved_publication_seconds *
                                                    1000,
                                            )}
                                        </dd>
                                    </div>
                                </>
                            )}
                        </dl>
                        <TechnicalDetails
                            items={[
                                {
                                    term: "Connection ID",
                                    description: (
                                        <ResourceId
                                            id={catalog.id}
                                            label="connection ID"
                                        />
                                    ),
                                },
                            ]}
                        />
                    </CardContent>
                </Card>
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
                                    <CardTitle>
                                        Effective storage profile
                                    </CardTitle>
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
                                        0 && (
                                        <p>No storage backends configured.</p>
                                    )}
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
                                                                ([
                                                                    key,
                                                                    value,
                                                                ]) =>
                                                                    key !==
                                                                        "kind" &&
                                                                    value !==
                                                                        null,
                                                            )
                                                            .map(
                                                                ([
                                                                    key,
                                                                    value,
                                                                ]) => (
                                                                    <div
                                                                        key={
                                                                            key
                                                                        }
                                                                        className={
                                                                            key ===
                                                                            "endpoint"
                                                                                ? "sm:col-span-2"
                                                                                : undefined
                                                                        }
                                                                    >
                                                                        <dt className="text-xs text-muted-foreground">
                                                                            {words(
                                                                                key,
                                                                            )}
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
                                                                ),
                                                            )}
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
                                    <CardTitle>
                                        Unresolved publications
                                    </CardTitle>
                                </CardHeader>
                                <CardContent>
                                    <dl className="m-0 grid grid-cols-2 gap-4">
                                        <div>
                                            <dt className="text-xs text-muted-foreground">
                                                Awaiting resolution
                                            </dt>
                                            <dd className="m-0 mt-1 text-xl font-semibold tabular-nums">
                                                {
                                                    catalog
                                                        .unresolved_publications
                                                        .count
                                                }
                                            </dd>
                                        </div>
                                        <div>
                                            <dt className="text-xs text-muted-foreground">
                                                Oldest age
                                            </dt>
                                            <dd className="m-0 mt-1 text-lg font-semibold tabular-nums">
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
                                    {catalog.unresolved_publications
                                        .overdue && (
                                        <p role="alert">
                                            The oldest publication exceeds this
                                            connection’s age bound. Investigate
                                            its outcome; the bound never
                                            declares it failed.
                                        </p>
                                    )}
                                </CardContent>
                            </Card>
                        )}
                    </div>
                </div>
            </TabsContent>
            <TabsContent value="tables">
                <RegisteredTables id={id} manager={manager} />
            </TabsContent>
            {access && (
                <TabsContent value="access">
                    <Access
                        scope={{ catalog_connection: id }}
                        connections={[id]}
                        scopeLabel={catalog.name}
                    />
                </TabsContent>
            )}
            {manager && (
                <TabsContent value="settings" keepMounted>
                    <Settings key={catalog.revision} catalog={catalog} />
                </TabsContent>
            )}
        </Tabs>
    );
}
