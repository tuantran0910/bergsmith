import { Link } from "@tanstack/react-router";

import { useJobs } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import { StatusBadge } from "../components/StatusBadge";
import { shortId, timestamp } from "../format";
import { executionStatus, publicationStatus } from "../jobs/status";

/** Jobs the session may see, with execution and publication side by side. */
export function JobsPage() {
    const jobs = useJobs();
    const items = jobs.data?.pages.flatMap((page) => page.items) ?? [];

    return (
        <section>
            <h1>Jobs</h1>
            {jobs.isPending && <p>Loading jobs…</p>}
            {jobs.error && (
                <ErrorNotice
                    error={jobs.error}
                    onRetry={() => {
                        void jobs.refetch();
                    }}
                />
            )}
            {jobs.isSuccess && items.length === 0 && (
                <p>No jobs are visible to you.</p>
            )}
            {items.length > 0 && (
                <table>
                    <thead>
                        <tr>
                            <th scope="col">Job</th>
                            <th scope="col">Table</th>
                            <th scope="col">State</th>
                            <th scope="col">Execution</th>
                            <th scope="col">Publication</th>
                            <th scope="col">Created</th>
                        </tr>
                    </thead>
                    <tbody>
                        {items.map((job) => (
                            <tr key={job.id}>
                                <td>
                                    <Link
                                        to="/jobs/$jobId"
                                        params={{ jobId: job.id }}
                                        title={job.id}
                                    >
                                        {shortId(job.id)}
                                    </Link>
                                </td>
                                <td>
                                    <Link
                                        to="/tables/$tableId"
                                        params={{ tableId: job.table_id }}
                                        title={job.table_id}
                                    >
                                        {shortId(job.table_id)}
                                    </Link>
                                </td>
                                <td>{job.state}</td>
                                <td>
                                    <StatusBadge
                                        status={executionStatus(job)}
                                    />
                                </td>
                                <td>
                                    <StatusBadge
                                        status={publicationStatus(job)}
                                    />
                                </td>
                                <td>{timestamp(job.created_at)}</td>
                            </tr>
                        ))}
                    </tbody>
                </table>
            )}
            {jobs.hasNextPage && (
                <button
                    type="button"
                    disabled={jobs.isFetchingNextPage}
                    onClick={() => {
                        void jobs.fetchNextPage();
                    }}
                >
                    Load more jobs
                </button>
            )}
        </section>
    );
}
