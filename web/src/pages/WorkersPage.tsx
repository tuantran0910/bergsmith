import { ApiFailure } from "../api/client";
import { useWorkers } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import { bytes, shortId, timestamp, words } from "../format";

/** Current worker sessions; requires organization-wide `workers:manage`. */
export function WorkersPage() {
    const workers = useWorkers();
    const items = workers.data?.pages.flatMap((page) => page.items) ?? [];

    if (workers.error instanceof ApiFailure && workers.error.status === 403) {
        return (
            <section>
                <h1>Workers</h1>
                <p role="alert">
                    Worker health requires an organization-wide grant with{" "}
                    <code>workers:manage</code>.
                </p>
            </section>
        );
    }

    return (
        <section>
            <h1>Workers</h1>
            {workers.isPending && <p>Loading workers…</p>}
            {workers.error && (
                <ErrorNotice
                    error={workers.error}
                    onRetry={() => {
                        void workers.refetch();
                    }}
                />
            )}
            {workers.isSuccess && items.length === 0 && (
                <p>No worker has registered yet.</p>
            )}
            {items.length > 0 && (
                <table>
                    <thead>
                        <tr>
                            <th scope="col">Worker</th>
                            <th scope="col">Pool</th>
                            <th scope="col">Status</th>
                            <th scope="col">Running attempt</th>
                            <th scope="col">Capacity</th>
                            <th scope="col">Build</th>
                            <th scope="col">Lease expires</th>
                        </tr>
                    </thead>
                    <tbody>
                        {items.map((worker) => (
                            <tr key={worker.id}>
                                <td title={worker.id}>{worker.worker_name}</td>
                                <td>{worker.pool}</td>
                                <td>{words(worker.status)}</td>
                                <td>
                                    {worker.running_attempt_id === null ? (
                                        "Idle"
                                    ) : (
                                        <code title={worker.running_attempt_id}>
                                            {shortId(worker.running_attempt_id)}
                                        </code>
                                    )}
                                </td>
                                <td>
                                    {bytes(worker.memory_bytes)},{" "}
                                    {worker.cpu_millis / 1000} CPU
                                </td>
                                <td>{worker.build_version}</td>
                                <td>{timestamp(worker.lease_expires_at)}</td>
                            </tr>
                        ))}
                    </tbody>
                </table>
            )}
            {workers.hasNextPage && (
                <button
                    type="button"
                    disabled={workers.isFetchingNextPage}
                    onClick={() => {
                        void workers.fetchNextPage();
                    }}
                >
                    Load more workers
                </button>
            )}
        </section>
    );
}
