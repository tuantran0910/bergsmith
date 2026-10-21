import { ApiFailure } from "../api/client";
import { useAudit } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import { timestamp, words } from "../format";

/** Organization audit history; requires organization-wide `audit:read`. */
export function AuditPage() {
    const audit = useAudit();
    const items = audit.data?.pages.flatMap((page) => page.items) ?? [];

    if (audit.error instanceof ApiFailure && audit.error.status === 403) {
        return (
            <section>
                <h1>Audit history</h1>
                <p role="alert">
                    Audit history requires an organization-wide grant with{" "}
                    <code>audit:read</code>.
                </p>
            </section>
        );
    }

    return (
        <section>
            <h1>Audit history</h1>
            {audit.isPending && <p>Loading audit history…</p>}
            {audit.error && (
                <ErrorNotice
                    error={audit.error}
                    onRetry={() => {
                        void audit.refetch();
                    }}
                />
            )}
            {audit.isSuccess && items.length === 0 && (
                <p>No audit events yet.</p>
            )}
            {items.length > 0 && (
                <table>
                    <thead>
                        <tr>
                            <th scope="col">When</th>
                            <th scope="col">Actor</th>
                            <th scope="col">Action</th>
                            <th scope="col">Resource</th>
                            <th scope="col">Outcome</th>
                            <th scope="col">Detail</th>
                        </tr>
                    </thead>
                    <tbody>
                        {items.map((event) => (
                            <tr key={event.id}>
                                <td>{timestamp(event.occurred_at)}</td>
                                <td>
                                    {event.actor_kind}
                                    {event.actor_principal_id !== null && (
                                        <>
                                            {" "}
                                            <code>
                                                {event.actor_principal_id}
                                            </code>
                                        </>
                                    )}
                                </td>
                                <td>{event.action}</td>
                                <td>
                                    {event.resource_type}
                                    {event.resource_id !== null && (
                                        <>
                                            {" "}
                                            <code>{event.resource_id}</code>
                                        </>
                                    )}
                                </td>
                                <td>{words(event.outcome)}</td>
                                <td>
                                    <code>{JSON.stringify(event.detail)}</code>
                                </td>
                            </tr>
                        ))}
                    </tbody>
                </table>
            )}
            {audit.hasNextPage && (
                <button
                    type="button"
                    disabled={audit.isFetchingNextPage}
                    onClick={() => {
                        void audit.fetchNextPage();
                    }}
                >
                    Load more events
                </button>
            )}
        </section>
    );
}
