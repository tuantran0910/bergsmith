import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { PlayIcon, ScanSearchIcon } from "lucide-react";

import { hasTablePermission } from "../api/auth";
import { api, call, csrfHeader } from "../api/client";
import { useKeyedMutation } from "../api/keyed";
import { useCatalogNames, useTable } from "../api/queries";
import { useBreadcrumbLabel } from "../components/Breadcrumbs";
import { ErrorNotice } from "../components/ErrorNotice";
import { HealthBadge } from "../components/HealthBadge";
import { Metric, MetricGrid } from "../components/Metric";
import { LoadingDetail, PageHeader } from "../components/Page";
import { ResourceId } from "../components/ResourceId";
import { TechnicalDetails } from "../components/TechnicalDetails";
import { Confirm, uncertain } from "../components/Writes";
import {
    aliasName,
    count,
    date,
    relativeTime,
    shortId,
    timestamp,
} from "../format";
import { useSession, useSessionMutation } from "../queryClient";
import { Button } from "../ui/Button";
import { Card, CardContent } from "../ui/Card";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../ui/Tabs";
import { PreviewView } from "./PreviewView";
import { Access } from "./access/Access";
import { canManageScope } from "./access/Grants";
import { namespaceKey, tab, type TableTab } from "./tables/search";
import { TableDefinition } from "./tables/Definition";
import { TableHealth } from "./tables/Health";
import { TableMaintenance } from "./tables/Maintenance";
import { TableStorage } from "./tables/Storage";
import { TableStatistics } from "./tables/Statistics";
import { TableSnapshots } from "./tables/Snapshots";

/** One table: its names and technical identity, then preview and run. */
export function TablePage({ id }: { id: string }) {
    const navigate = useNavigate();
    const search = useSearch({ from: "/tables/$tableId" });
    const current: TableTab = tab(search.tab) ?? "overview";
    const table = useTable(id);
    const catalogs = useCatalogNames();
    const names = catalogs.data ?? new Map<string, string>();
    const session = useSession();
    const [primaryAlias] = table.data?.aliases ?? [];
    const access =
        session !== null &&
        canManageScope(
            session,
            { table: id },
            (table.data?.aliases ?? []).map((alias) => alias.catalog_id),
        );
    const shown: TableTab =
        current === "access" && !access ? "overview" : current;
    useBreadcrumbLabel(
        table.data === undefined
            ? undefined
            : primaryAlias === undefined
              ? `Table ${shortId(table.data.id)}`
              : aliasName(primaryAlias, names),
    );

    const preview = useSessionMutation({
        mutationFn: () =>
            call(
                api.POST("/api/v1/previews", {
                    params: { header: csrfHeader() },
                    body: { table_id: id },
                }),
            ),
    });

    const expire = useKeyedMutation(
        (key: string) =>
            call(
                api.POST("/api/v1/jobs", {
                    params: {
                        header: { "Idempotency-Key": key, ...csrfHeader() },
                    },
                    body: { table_id: id, operation: "expire_snapshots" },
                }),
            ),
        (accepted) => {
            void navigate({
                to: "/jobs/$jobId",
                params: { jobId: accepted.job_id },
            });
        },
    );
    const cleanup = useKeyedMutation(
        (key: string) =>
            call(
                api.POST("/api/v1/jobs", {
                    params: {
                        header: { "Idempotency-Key": key, ...csrfHeader() },
                    },
                    body: { table_id: id, operation: "remove_orphan_files" },
                }),
            ),
        (accepted) => {
            void navigate({
                to: "/jobs/$jobId",
                params: { jobId: accepted.job_id },
            });
        },
    );
    const rewriteManifests = useKeyedMutation(
        (key: string) =>
            call(
                api.POST("/api/v1/jobs", {
                    params: {
                        header: { "Idempotency-Key": key, ...csrfHeader() },
                    },
                    body: { table_id: id, operation: "rewrite_manifests" },
                }),
            ),
        (accepted) => {
            void navigate({
                to: "/jobs/$jobId",
                params: { jobId: accepted.job_id },
            });
        },
    );
    const removeDangling = useKeyedMutation(
        (key: string) =>
            call(
                api.POST("/api/v1/jobs", {
                    params: {
                        header: { "Idempotency-Key": key, ...csrfHeader() },
                    },
                    body: {
                        table_id: id,
                        operation: "remove_dangling_deletes",
                    },
                }),
            ),
        (accepted) => {
            void navigate({
                to: "/jobs/$jobId",
                params: { jobId: accepted.job_id },
            });
        },
    );
    const submit = useKeyedMutation(
        (key: string) =>
            call(
                api.POST("/api/v1/jobs", {
                    params: {
                        header: { "Idempotency-Key": key, ...csrfHeader() },
                    },
                    body: { table_id: id },
                }),
            ),
        (accepted) => {
            void navigate({
                to: "/jobs/$jobId",
                params: { jobId: accepted.job_id },
            });
        },
    );

    if (table.isPending) {
        return <LoadingDetail label="Loading table…" />;
    }
    if (table.error) {
        return (
            <ErrorNotice
                error={table.error}
                onRetry={() => {
                    void table.refetch();
                }}
            />
        );
    }
    const [primary] = table.data.aliases;
    const fullName = primary
        ? aliasName(primary, names)
        : `Table ${shortId(table.data.id)}`;
    const catalogLinks = [
        ...new Map(
            table.data.aliases.map((alias) => [
                alias.catalog_id,
                names.get(alias.catalog_id) ?? alias.catalog_id,
            ]),
        ).entries(),
    ];
    const namespaces = [
        ...new Map(
            table.data.aliases.map((alias) => [
                namespaceKey(alias.namespace),
                alias.namespace,
            ]),
        ).values(),
    ];
    const canSubmit =
        session !== null &&
        hasTablePermission(
            session,
            "jobs:submit",
            table.data.id,
            table.data.aliases.map((alias) => alias.catalog_id),
        );

    return (
        <section>
            <PageHeader
                title={fullName}
                description={
                    <span className="inline-flex flex-wrap items-center gap-2">
                        <HealthBadge state={table.data.health.state} />
                        <span>
                            A preview reads current table metadata and creates
                            no job. Running maintenance plans again from the
                            table as it is then.
                        </span>
                    </span>
                }
                actions={
                    canSubmit ? (
                        <>
                            <Button
                                variant="outline"
                                disabled={preview.isPending}
                                onClick={() => {
                                    preview.mutate();
                                }}
                            >
                                <ScanSearchIcon aria-hidden="true" />
                                {preview.isPending
                                    ? "Previewing…"
                                    : "Preview maintenance"}
                            </Button>
                            <Button
                                disabled={submit.isPending}
                                onClick={() => {
                                    submit.mutate(undefined);
                                }}
                            >
                                <PlayIcon aria-hidden="true" />
                                {submit.isPending
                                    ? "Submitting…"
                                    : "Run maintenance"}
                            </Button>
                            <Confirm
                                label="Remove orphan files"
                                description="Deletes storage objects no retained snapshot references, outside Bergsmith's own prefix, older than the deployment's minimum age and the connection's protection window. It re-checks every object against current table metadata before deleting. This cannot be undone, and the deployment must enable orphan cleanup."
                                pending={
                                    cleanup.isPending ||
                                    uncertain(cleanup.error)
                                }
                                error={cleanup.error}
                                onConfirm={() => {
                                    cleanup.mutate(undefined);
                                }}
                            />
                            <Confirm
                                label="Rewrite manifests"
                                description="Merges the small data manifests of the table's current snapshot into target-sized ones in one new snapshot. No data or delete file changes. It is refused while a compaction of this table is unfinished, and the deployment must enable manifest rewrite."
                                pending={
                                    rewriteManifests.isPending ||
                                    uncertain(rewriteManifests.error)
                                }
                                error={rewriteManifests.error}
                                onConfirm={() => {
                                    rewriteManifests.mutate(undefined);
                                }}
                            />
                            <Confirm
                                label="Remove dangling deletes"
                                description="Removes delete files that apply to no live data file of the current snapshot, in one new snapshot. Reads return the same rows. Files stay in storage until snapshot expiration and orphan cleanup. It is refused while a compaction of this table is unfinished, and the deployment must enable dangling-delete removal."
                                pending={
                                    removeDangling.isPending ||
                                    uncertain(removeDangling.error)
                                }
                                error={removeDangling.error}
                                onConfirm={() => {
                                    removeDangling.mutate(undefined);
                                }}
                            />
                            <Confirm
                                label="Expire snapshots"
                                description="Removes snapshot entries older than the table's retention settings and the connection's execution and publication bounds. Running jobs' snapshots, every branch and tag, and anything newer are kept. No data or metadata files are deleted."
                                pending={
                                    expire.isPending || uncertain(expire.error)
                                }
                                error={expire.error}
                                onConfirm={() => {
                                    expire.mutate(undefined);
                                }}
                            />
                        </>
                    ) : undefined
                }
            />

            <Tabs
                value={shown}
                onValueChange={(next) => {
                    const selected = tab(next);
                    void navigate({
                        to: "/tables/$tableId",
                        params: { tableId: id },
                        search:
                            selected === undefined || selected === "overview"
                                ? {}
                                : { tab: selected },
                        replace: true,
                    });
                }}
                className="mb-6"
            >
                <TabsList>
                    <TabsTrigger value="overview">Overview</TabsTrigger>
                    <TabsTrigger value="definition">Definition</TabsTrigger>
                    <TabsTrigger value="health">Health</TabsTrigger>
                    <TabsTrigger value="snapshots">Snapshots</TabsTrigger>
                    <TabsTrigger value="statistics">Statistics</TabsTrigger>
                    <TabsTrigger value="storage">Storage</TabsTrigger>
                    <TabsTrigger value="maintenance">Maintenance</TabsTrigger>
                    {access && <TabsTrigger value="access">Access</TabsTrigger>}
                </TabsList>
                <TabsContent value="overview">
                    <MetricGrid className="mb-6">
                        <Metric
                            label="Registered"
                            value={date(table.data.created_at)}
                            detail={relativeTime(table.data.created_at)}
                        />
                        <Metric
                            label="Registered names"
                            value={table.data.aliases.length}
                            detail={`Across ${count(catalogLinks.length, "catalog")}`}
                        />
                        <Metric
                            label="Catalogs"
                            value={catalogLinks.length}
                            detail={
                                catalogLinks.length === 0
                                    ? "None"
                                    : catalogLinks
                                          .map(([, name]) => name)
                                          .join(", ")
                            }
                        />
                        <Metric
                            label="Namespaces"
                            value={namespaces.length}
                            detail={
                                namespaces.length === 0
                                    ? "None"
                                    : namespaces
                                          .map((levels) => levels.join("."))
                                          .join(", ")
                            }
                        />
                    </MetricGrid>

                    <Card className="mb-6">
                        <CardContent>
                            <TechnicalDetails
                                className="border-t-0 pt-0"
                                items={[
                                    {
                                        term: "Table UUID",
                                        description: (
                                            <ResourceId
                                                id={table.data.table_uuid}
                                                label="table UUID"
                                            />
                                        ),
                                    },
                                    {
                                        term: "Registered names",
                                        description:
                                            table.data.aliases.length === 0 ? (
                                                "None"
                                            ) : (
                                                <ul className="m-0 grid list-none gap-1 p-0">
                                                    {table.data.aliases.map(
                                                        (alias) => (
                                                            <li
                                                                key={JSON.stringify(
                                                                    [
                                                                        alias.catalog_id,
                                                                        alias.namespace,
                                                                        alias.name,
                                                                    ],
                                                                )}
                                                            >
                                                                <code>
                                                                    {aliasName(
                                                                        alias,
                                                                        names,
                                                                    )}
                                                                </code>{" "}
                                                                <span className="text-xs text-muted-foreground">
                                                                    revision{" "}
                                                                    {
                                                                        alias.revision
                                                                    }
                                                                </span>
                                                            </li>
                                                        ),
                                                    )}
                                                </ul>
                                            ),
                                    },
                                    {
                                        term: "Catalog connections",
                                        description:
                                            catalogLinks.length === 0 ? (
                                                "None"
                                            ) : (
                                                <ul className="m-0 grid list-none gap-1 p-0">
                                                    {catalogLinks.map(
                                                        ([id, name]) => (
                                                            <li key={id}>
                                                                <Link
                                                                    to="/catalogs/$catalogId"
                                                                    params={{
                                                                        catalogId:
                                                                            id,
                                                                    }}
                                                                >
                                                                    {name}
                                                                </Link>
                                                            </li>
                                                        ),
                                                    )}
                                                </ul>
                                            ),
                                    },
                                    {
                                        term: "Registered",
                                        description: (
                                            <time
                                                dateTime={table.data.created_at}
                                            >
                                                {timestamp(
                                                    table.data.created_at,
                                                )}
                                            </time>
                                        ),
                                    },
                                ]}
                            />
                        </CardContent>
                    </Card>

                    {submit.error !== null && (
                        <ErrorNotice
                            error={submit.error}
                            onRetry={() => {
                                submit.mutate(undefined);
                            }}
                        />
                    )}
                    {preview.error !== null && (
                        <ErrorNotice
                            error={preview.error}
                            onRetry={() => {
                                preview.mutate();
                            }}
                        />
                    )}
                    {preview.data && <PreviewView preview={preview.data} />}
                </TabsContent>
                <TabsContent value="definition">
                    <TableDefinition id={id} />
                </TabsContent>
                <TabsContent value="health">
                    <TableHealth id={id} />
                </TabsContent>
                <TabsContent value="snapshots">
                    <TableSnapshots id={id} />
                </TabsContent>
                <TabsContent value="statistics">
                    <TableStatistics id={id} />
                </TabsContent>
                <TabsContent value="storage">
                    <TableStorage id={id} />
                </TabsContent>
                <TabsContent value="maintenance">
                    <TableMaintenance id={id} />
                </TabsContent>
                {access && (
                    <TabsContent value="access">
                        <Access
                            scope={{ table: id }}
                            scopeLabel={fullName}
                            connections={table.data.aliases.map(
                                (alias) => alias.catalog_id,
                            )}
                        />
                    </TabsContent>
                )}
            </Tabs>
        </section>
    );
}
