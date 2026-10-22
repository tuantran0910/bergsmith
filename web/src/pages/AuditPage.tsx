import { ScrollTextIcon } from "lucide-react";

import { ApiFailure, type Schemas } from "../api/client";
import { useAudit } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import { GrantRequired } from "../components/GrantRequired";
import {
    EmptyState,
    GridFrame,
    LoadMore,
    LoadingRows,
    PageHeader,
} from "../components/Page";
import { StatusBadge } from "../components/StatusBadge";
import { timestamp, words } from "../format";
import type { Tone } from "../jobs/status";
import {
    Table,
    TableBody,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "../ui/Table";

const OUTCOME_TONES: Record<Schemas["AuditEvent"]["outcome"], Tone> = {
    Allowed: "ok",
    Denied: "warning",
    Failed: "error",
};

const DESCRIPTION =
    "Who did what in this organization, including refused requests.";

/** Organization audit history; requires organization-wide `audit:read`. */
export function AuditPage() {
    const audit = useAudit();
    const items = audit.data?.pages.flatMap((page) => page.items) ?? [];

    if (audit.error instanceof ApiFailure && audit.error.status === 403) {
        return (
            <section>
                <PageHeader title="Audit history" description={DESCRIPTION} />
                <GrantRequired>
                    Audit history requires an organization-wide grant with{" "}
                    <code>audit:read</code>.
                </GrantRequired>
            </section>
        );
    }

    return (
        <section>
            <PageHeader title="Audit history" description={DESCRIPTION} />
            {audit.isPending && <LoadingRows label="Loading audit history…" />}
            {audit.error && (
                <ErrorNotice
                    error={audit.error}
                    onRetry={() => {
                        void audit.refetch();
                    }}
                />
            )}
            {audit.isSuccess && items.length === 0 && (
                <EmptyState
                    icon={ScrollTextIcon}
                    title="No audit events yet."
                />
            )}
            {items.length > 0 && (
                <>
                    <GridFrame>
                        <Table>
                            <TableHeader>
                                <TableRow>
                                    <TableHead className="pl-4">When</TableHead>
                                    <TableHead>Actor</TableHead>
                                    <TableHead>Action</TableHead>
                                    <TableHead>Resource</TableHead>
                                    <TableHead>Outcome</TableHead>
                                    <TableHead className="pr-4">
                                        Detail
                                    </TableHead>
                                </TableRow>
                            </TableHeader>
                            <TableBody>
                                {items.map((event) => (
                                    <TableRow key={event.id}>
                                        <TableCell className="pl-4 whitespace-nowrap text-muted-foreground">
                                            {timestamp(event.occurred_at)}
                                        </TableCell>
                                        <TableCell>
                                            {event.actor_kind}
                                            {event.actor_principal_id !==
                                                null && (
                                                <>
                                                    {" "}
                                                    <code>
                                                        {
                                                            event.actor_principal_id
                                                        }
                                                    </code>
                                                </>
                                            )}
                                        </TableCell>
                                        <TableCell>
                                            <code>{event.action}</code>
                                        </TableCell>
                                        <TableCell>
                                            {event.resource_type}
                                            {event.resource_id !== null && (
                                                <>
                                                    {" "}
                                                    <code>
                                                        {event.resource_id}
                                                    </code>
                                                </>
                                            )}
                                        </TableCell>
                                        <TableCell>
                                            <StatusBadge
                                                status={{
                                                    label: words(event.outcome),
                                                    tone: OUTCOME_TONES[
                                                        event.outcome
                                                    ],
                                                }}
                                            />
                                        </TableCell>
                                        <TableCell className="pr-4">
                                            <code className="text-muted-foreground">
                                                {JSON.stringify(event.detail)}
                                            </code>
                                        </TableCell>
                                    </TableRow>
                                ))}
                            </TableBody>
                        </Table>
                    </GridFrame>
                    <LoadMore
                        shown={items.length}
                        noun="events"
                        hasMore={audit.hasNextPage}
                        loading={audit.isFetchingNextPage}
                        onLoad={() => {
                            void audit.fetchNextPage();
                        }}
                    />
                </>
            )}
        </section>
    );
}
