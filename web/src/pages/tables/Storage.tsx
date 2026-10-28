import { useTableStorage } from "../../api/storage";
import { ErrorNotice } from "../../components/ErrorNotice";
import { StorageBreakdown } from "../../components/StorageBreakdown";
import { relativeTime, date } from "../../format";
import { Card, CardContent, CardHeader, CardTitle } from "../../ui/Card";
import { Skeleton } from "../../ui/Skeleton";
export function TableStorage({ id }: { id: string }) {
    const query = useTableStorage(id);
    if (query.isPending) return <Skeleton className="h-24 w-full" />;
    if (query.error)
        return (
            <ErrorNotice
                error={query.error}
                onRetry={() => {
                    void query.refetch();
                }}
            />
        );
    const { latest, enabled, failure_reason, checked_at } = query.data;
    return (
        <Card>
            <CardHeader>
                <CardTitle>Storage breakdown</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-4">
                {!enabled && (
                    <p>Storage collection is disabled for this deployment.</p>
                )}
                {failure_reason !== null && (
                    <p role="status">
                        The latest check failed ({failure_reason}).{" "}
                        {checked_at === null
                            ? ""
                            : `Checked ${relativeTime(checked_at)}.`}{" "}
                        Any previous complete sample is retained.
                    </p>
                )}
                {latest === null ? (
                    <p>No complete storage sample yet. Usage is unknown.</p>
                ) : (
                    <>
                        <p>
                            {latest.stale ? "Stale sample" : "Sampled"}{" "}
                            {relativeTime(latest.sampled_at)}. This is an
                            observation, not an atomic storage snapshot.
                        </p>
                        <p>
                            Orphan candidates were last modified by{" "}
                            {date(latest.cutoff_at)} and satisfy the cleanup
                            protection rules at collection time. This is not a
                            promise of reclaimable space.
                        </p>
                        <StorageBreakdown buckets={latest} />
                    </>
                )}
            </CardContent>
        </Card>
    );
}
