import { ScrollTextIcon } from "lucide-react";
import { Link } from "@tanstack/react-router";

import { ApiFailure, type Schemas } from "../api/client";
import { useAudit } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import { GrantRequired } from "../components/GrantRequired";
import { CopyButton } from "../components/ResourceId";
import {
    EmptyState,
    GridFrame,
    LoadMore,
    LoadingRows,
    PageHeader,
} from "../components/Page";
import { StatusBadge } from "../components/StatusBadge";
import { shortId, timestamp, words } from "../format";
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

/** The actor by display name and kind, with the principal ID in the title and
 * a copy button; events without a principal show only the kind.
 */
function Actor({ event }: { event: Schemas["AuditEvent"] }) {
    const id = event.actor_principal_id;
    if (id === null) {
        return <span>{event.actor_kind}</span>;
    }
    return (
        <span className="inline-flex flex-col">
            <span className="inline-flex items-center gap-1">
                <span title={id} className="font-semibold">
                    {event.actor_display_name ?? shortId(id)}
                </span>
                <CopyButton value={id} label="principal ID" />
            </span>
            <span className="text-xs text-muted-foreground">
                {event.actor_kind}
            </span>
        </span>
    );
}

/**
 * The resource type leads; a short ID and copy button stay with it because
 * audit events do not carry a resource display name. Known resources link to
 * their detail pages.
 */
function Resource({ event }: { event: Schemas["AuditEvent"] }) {
    const id = event.resource_id;
    const type = words(event.resource_type);
    if (id === null) {
        return <span className="text-muted-foreground">{type}</span>;
    }
    const short = <code title={id}>{shortId(id)}</code>;
    const linked =
        event.resource_type === "job" ? (
            <Link
                to="/jobs/$jobId"
                params={{ jobId: id }}
                title={id}
                aria-label={`${type} ${shortId(id)}`}
            >
                {short}
            </Link>
        ) : event.resource_type === "catalog" ? (
            <Link
                to="/catalogs/$catalogId"
                params={{ catalogId: id }}
                title={id}
                aria-label={`${type} ${shortId(id)}`}
            >
                {short}
            </Link>
        ) : event.resource_type === "table" ? (
            <Link
                to="/tables/$tableId"
                params={{ tableId: id }}
                title={id}
                aria-label={`${type} ${shortId(id)}`}
            >
                {short}
            </Link>
        ) : (
            short
        );
    return (
        <span className="inline-flex flex-col">
            <span className="font-semibold">{type}</span>
            <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
                {linked}
                <CopyButton value={id} label={`${type.toLowerCase()} ID`} />
            </span>
            <span className="sr-only">
                {type} ID {id}
            </span>
        </span>
    );
}

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
                                            <Actor event={event} />
                                        </TableCell>
                                        <TableCell>
                                            <code>{event.action}</code>
                                        </TableCell>
                                        <TableCell>
                                            <Resource event={event} />
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
