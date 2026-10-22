import { ServerIcon } from "lucide-react";

import { ApiFailure, type Schemas } from "../api/client";
import { useWorkers } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import { GrantRequired } from "../components/GrantRequired";
import {
    EmptyState,
    GridFrame,
    LoadMore,
    LoadingRows,
    PageHeader,
} from "../components/Page";
import { ResourceId } from "../components/ResourceId";
import { StatusBadge } from "../components/StatusBadge";
import { bytes, timestamp, words } from "../format";
import type { Tone } from "../jobs/status";
import {
    Table,
    TableBody,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "../ui/Table";

const STATUS_TONES: Record<Schemas["WorkerSession"]["status"], Tone> = {
    live: "ok",
    draining: "warning",
    expired: "neutral",
};

const DESCRIPTION =
    "Worker sessions and their leases. A worker that stops renewing its lease expires.";

/** Current worker sessions; requires organization-wide `workers:manage`. */
export function WorkersPage() {
    const workers = useWorkers();
    const items = workers.data?.pages.flatMap((page) => page.items) ?? [];

    if (workers.error instanceof ApiFailure && workers.error.status === 403) {
        return (
            <section>
                <PageHeader title="Workers" description={DESCRIPTION} />
                <GrantRequired>
                    Worker health requires an organization-wide grant with{" "}
                    <code>workers:manage</code>.
                </GrantRequired>
            </section>
        );
    }

    return (
        <section>
            <PageHeader title="Workers" description={DESCRIPTION} />
            {workers.isPending && <LoadingRows label="Loading workers…" />}
            {workers.error && (
                <ErrorNotice
                    error={workers.error}
                    onRetry={() => {
                        void workers.refetch();
                    }}
                />
            )}
            {workers.isSuccess && items.length === 0 && (
                <EmptyState
                    icon={ServerIcon}
                    title="No worker has registered yet."
                >
                    A worker appears here once it registers with the
                    coordinator.
                </EmptyState>
            )}
            {items.length > 0 && (
                <>
                    <GridFrame>
                        <Table>
                            <TableHeader>
                                <TableRow>
                                    <TableHead className="pl-4">
                                        Worker
                                    </TableHead>
                                    <TableHead>Pool</TableHead>
                                    <TableHead>Status</TableHead>
                                    <TableHead>Running attempt</TableHead>
                                    <TableHead>Capacity</TableHead>
                                    <TableHead>Build</TableHead>
                                    <TableHead className="pr-4">
                                        Lease expires
                                    </TableHead>
                                </TableRow>
                            </TableHeader>
                            <TableBody>
                                {items.map((worker) => (
                                    <TableRow key={worker.id}>
                                        <TableCell
                                            className="pl-4 font-medium"
                                            title={worker.id}
                                        >
                                            {worker.worker_name}
                                        </TableCell>
                                        <TableCell>{worker.pool}</TableCell>
                                        <TableCell>
                                            <StatusBadge
                                                status={{
                                                    label: words(worker.status),
                                                    tone: STATUS_TONES[
                                                        worker.status
                                                    ],
                                                }}
                                            />
                                        </TableCell>
                                        <TableCell>
                                            {worker.running_attempt_id ===
                                            null ? (
                                                <span className="text-muted-foreground">
                                                    Idle
                                                </span>
                                            ) : (
                                                <ResourceId
                                                    id={
                                                        worker.running_attempt_id
                                                    }
                                                    label="attempt ID"
                                                    short
                                                />
                                            )}
                                        </TableCell>
                                        <TableCell>
                                            {bytes(worker.memory_bytes)},{" "}
                                            {worker.cpu_millis / 1000} CPU
                                        </TableCell>
                                        <TableCell>
                                            <code>{worker.build_version}</code>
                                        </TableCell>
                                        <TableCell className="pr-4 whitespace-nowrap text-muted-foreground">
                                            {timestamp(worker.lease_expires_at)}
                                        </TableCell>
                                    </TableRow>
                                ))}
                            </TableBody>
                        </Table>
                    </GridFrame>
                    <LoadMore
                        shown={items.length}
                        noun="workers"
                        hasMore={workers.hasNextPage}
                        loading={workers.isFetchingNextPage}
                        onLoad={() => {
                            void workers.fetchNextPage();
                        }}
                    />
                </>
            )}
        </section>
    );
}
