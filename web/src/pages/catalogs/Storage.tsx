import { useCatalogStorage } from "../../api/storage";
import { ErrorNotice } from "../../components/ErrorNotice";
import { StorageBreakdown } from "../../components/StorageBreakdown";
import { exactCount, relativeTime } from "../../format";
import { Card, CardContent, CardHeader, CardTitle } from "../../ui/Card";
import { Skeleton } from "../../ui/Skeleton";
export function CatalogStorage({ id }: { id: string }) {
    const query = useCatalogStorage(id);
    return (
        <Card>
            <CardHeader>
                <CardTitle>Storage breakdown</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-4">
                {query.isPending && <Skeleton className="h-24 w-full" />}
                {query.error && (
                    <ErrorNotice
                        error={query.error}
                        onRetry={() => {
                            void query.refetch();
                        }}
                    />
                )}
                {query.data && (
                    <>
                        {!query.data.enabled && (
                            <p>
                                Storage collection is disabled for this
                                deployment.
                            </p>
                        )}
                        <p>
                            {exactCount(query.data.assessed)} of{" "}
                            {exactCount(query.data.tables)} visible tables
                            assessed; {exactCount(query.data.fresh)} fresh;{" "}
                            {exactCount(query.data.stale)} stale;{" "}
                            {exactCount(query.data.failed)} latest checks
                            failed.
                        </p>
                        <p>
                            These are sums of sampled table footprints. Shared
                            files and overlapping table locations may count more
                            than once. This is not a bucket-wide physical total.
                        </p>
                        {query.data.assessed === "0" ? (
                            <p>
                                No complete storage samples yet. Usage is
                                unknown.
                            </p>
                        ) : (
                            <>
                                {query.data.oldest_sampled_at !== null &&
                                    query.data.newest_sampled_at !== null && (
                                        <p>
                                            Samples range from{" "}
                                            {relativeTime(
                                                query.data.oldest_sampled_at,
                                            )}{" "}
                                            to{" "}
                                            {relativeTime(
                                                query.data.newest_sampled_at,
                                            )}
                                            .
                                        </p>
                                    )}
                                <StorageBreakdown buckets={query.data} />
                            </>
                        )}
                    </>
                )}
            </CardContent>
        </Card>
    );
}
