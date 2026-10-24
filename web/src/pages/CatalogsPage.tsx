import { Link } from "@tanstack/react-router";
import { DatabaseIcon, PlusIcon } from "lucide-react";
import { canManageCatalog } from "../api/auth";
import { useCatalogs } from "../api/catalogs";
import { ErrorNotice } from "../components/ErrorNotice";
import {
    EmptyState,
    LoadMore,
    LoadingRows,
    PageHeader,
} from "../components/Page";
import { useSession } from "../queryClient";
import { Badge } from "../ui/Badge";
import { Card, CardContent } from "../ui/Card";
import { buttonVariants } from "../ui/Button";

/** Authorized connections, including paused and disabled connections. */
export function CatalogsPage() {
    const catalogs = useCatalogs();
    const session = useSession();
    const items = catalogs.data?.pages.flatMap((page) => page.items) ?? [];
    return (
        <section>
            <PageHeader
                title="Catalogs"
                description="Connect Iceberg catalogs and choose the tables to maintain."
                actions={
                    session !== null &&
                    canManageCatalog(session) && (
                        <Link
                            to="/catalogs/new"
                            className={buttonVariants({
                                size: "lg",
                                className: "hover:no-underline",
                            })}
                        >
                            <PlusIcon aria-hidden="true" />
                            Add Connection
                        </Link>
                    )
                }
            />
            {catalogs.isPending && <LoadingRows label="Loading catalogs…" />}
            {catalogs.error && (
                <ErrorNotice
                    error={catalogs.error}
                    onRetry={() => {
                        void catalogs.refetch();
                    }}
                />
            )}
            {catalogs.isSuccess && items.length === 0 && (
                <EmptyState
                    icon={DatabaseIcon}
                    title="No catalogs are visible to you."
                >
                    An administrator can add a connection or grant access to an
                    existing one.
                </EmptyState>
            )}
            <div className="grid gap-3">
                {items.map((catalog) => (
                    <Card key={catalog.id}>
                        <CardContent className="flex flex-wrap items-center justify-between gap-x-6 gap-y-3">
                            <div className="grid gap-1">
                                <Link
                                    to="/catalogs/$catalogId"
                                    params={{ catalogId: catalog.id }}
                                    className="text-base font-medium"
                                >
                                    {catalog.name}
                                </Link>
                                <p className="m-0 text-sm text-muted-foreground">
                                    Execution budget:{" "}
                                    {catalog.settings.max_execution_seconds} s
                                </p>
                            </div>
                            <div className="flex flex-wrap gap-2">
                                <Badge>
                                    {catalog.enabled ? "Enabled" : "Disabled"}
                                </Badge>
                                <Badge>
                                    {catalog.settings.maintenance_enabled
                                        ? "Maintenance on"
                                        : "Maintenance paused"}
                                </Badge>
                            </div>
                        </CardContent>
                    </Card>
                ))}
            </div>
            {catalogs.isSuccess && (
                <LoadMore
                    shown={items.length}
                    noun="catalogs"
                    hasMore={catalogs.hasNextPage}
                    loading={catalogs.isFetchingNextPage}
                    onLoad={() => {
                        void catalogs.fetchNextPage();
                    }}
                />
            )}
        </section>
    );
}
