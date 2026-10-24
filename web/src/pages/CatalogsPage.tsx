import { Link } from "@tanstack/react-router";
import { DatabaseIcon } from "lucide-react";
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
                        <Link to="/catalogs/new">Add connection</Link>
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
            <div className="grid gap-4 sm:grid-cols-2">
                {items.map((catalog) => (
                    <Card key={catalog.id}>
                        <CardContent className="grid gap-3">
                            <Link
                                to="/catalogs/$catalogId"
                                params={{ catalogId: catalog.id }}
                                className="text-base font-medium"
                            >
                                {catalog.name}
                            </Link>
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
                            <p className="m-0 text-muted-foreground">
                                Execution budget:{" "}
                                {catalog.settings.max_execution_seconds} s
                            </p>
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
