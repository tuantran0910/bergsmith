import { Link, useNavigate } from "@tanstack/react-router";
import { PlayIcon, ScanSearchIcon } from "lucide-react";

import { hasTablePermission } from "../api/auth";
import { api, call, csrfHeader } from "../api/client";
import { useKeyedMutation } from "../api/keyed";
import { useCatalogNames, useTable } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import { DefinitionList, Metric, MetricGrid } from "../components/Metric";
import { LoadingDetail, PageHeader } from "../components/Page";
import { ResourceId } from "../components/ResourceId";
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
import { Card, CardContent, CardHeader, CardTitle } from "../ui/Card";
import { PreviewView } from "./PreviewView";
import { Access } from "./access/Access";

/** One table: its names and technical identity, then preview and run. */
export function TablePage({ id }: { id: string }) {
    const navigate = useNavigate();
    const table = useTable(id);
    const catalogs = useCatalogNames();
    const names = catalogs.data ?? new Map<string, string>();
    const session = useSession();

    const preview = useSessionMutation({
        mutationFn: () =>
            call(
                api.POST("/api/v1/previews", {
                    params: { header: csrfHeader() },
                    body: { table_id: id },
                }),
            ),
    });

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
        ...new Set(
            table.data.aliases.map((alias) => alias.namespace.join(".")),
        ),
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
                description="A preview reads current table metadata and creates no job. Running maintenance plans again from the table as it is then."
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
                        </>
                    ) : undefined
                }
            />

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
                            : catalogLinks.map(([, name]) => name).join(", ")
                    }
                />
                <Metric
                    label="Namespaces"
                    value={namespaces.length}
                    detail={
                        namespaces.length === 0 ? "None" : namespaces.join(", ")
                    }
                />
            </MetricGrid>

            <Card className="mb-6">
                <CardHeader>
                    <CardTitle>Technical details</CardTitle>
                </CardHeader>
                <CardContent>
                    <DefinitionList
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
                                            {table.data.aliases.map((alias) => (
                                                <li
                                                    key={`${alias.catalog_id}-${alias.namespace.join(".")}-${alias.name}`}
                                                >
                                                    <code>
                                                        {aliasName(
                                                            alias,
                                                            names,
                                                        )}
                                                    </code>{" "}
                                                    <span className="text-xs text-muted-foreground">
                                                        revision{" "}
                                                        {alias.revision}
                                                    </span>
                                                </li>
                                            ))}
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
                                            {catalogLinks.map(([id, name]) => (
                                                <li key={id}>
                                                    <Link
                                                        to="/catalogs/$catalogId"
                                                        params={{
                                                            catalogId: id,
                                                        }}
                                                    >
                                                        {name}
                                                    </Link>
                                                </li>
                                            ))}
                                        </ul>
                                    ),
                            },
                            {
                                term: "Registered",
                                description: (
                                    <time dateTime={table.data.created_at}>
                                        {timestamp(table.data.created_at)}
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
            <Access
                scope={{ table: id }}
                scopeLabel={fullName}
                connections={table.data.aliases.map(
                    (alias) => alias.catalog_id,
                )}
            />
        </section>
    );
}
