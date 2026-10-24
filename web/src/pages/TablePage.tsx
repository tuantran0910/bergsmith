import { useNavigate } from "@tanstack/react-router";
import { PlayIcon, ScanSearchIcon } from "lucide-react";

import { hasTablePermission } from "../api/auth";
import { api, call, csrfHeader } from "../api/client";
import { useKeyedMutation } from "../api/keyed";
import { useCatalogNames, useTable } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import { LoadingDetail, PageHeader } from "../components/Page";
import { ResourceId } from "../components/ResourceId";
import { aliasName, timestamp } from "../format";
import { useSession, useSessionMutation } from "../queryClient";
import { Button } from "../ui/Button";
import { Card, CardContent } from "../ui/Card";
import { PreviewView } from "./PreviewView";
import { Access } from "./access/Access";

/** One table: preview what maintenance would do, then run it. */
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
                title={primary ? aliasName(primary, names) : table.data.id}
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

            <Card>
                <CardContent>
                    <dl className="m-0 grid gap-x-6 gap-y-3 sm:grid-cols-[max-content_1fr]">
                        <dt className="text-muted-foreground">Table UUID</dt>
                        <dd className="m-0">
                            <ResourceId
                                id={table.data.table_uuid}
                                label="table UUID"
                            />
                        </dd>
                        <dt className="text-muted-foreground">Names</dt>
                        <dd className="m-0">
                            <ul className="m-0 grid list-none gap-1 p-0">
                                {table.data.aliases.map((alias) => {
                                    const name = aliasName(alias, names);
                                    return (
                                        <li key={name}>
                                            <code>{name}</code>
                                        </li>
                                    );
                                })}
                            </ul>
                        </dd>
                        <dt className="text-muted-foreground">Registered</dt>
                        <dd className="m-0">
                            {timestamp(table.data.created_at)}
                        </dd>
                    </dl>
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
                scopeLabel={primary ? aliasName(primary, names) : id}
                connections={table.data.aliases.map(
                    (alias) => alias.catalog_id,
                )}
            />
        </section>
    );
}
