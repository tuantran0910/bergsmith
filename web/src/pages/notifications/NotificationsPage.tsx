// Notification destinations: where events are delivered. The page lists the
// organization's configuration, sends a test through the coordinator, and
// edits each row against the revision it read.

import { BellRingIcon, PlusIcon } from "lucide-react";
import { useState } from "react";

import { api, call, csrfHeader, type Schemas } from "../../api/client";
import { canManageNotifications } from "../../api/auth";
import { useNotificationDestinations } from "../../api/notifications";
import { DataTable, type DataTableColumn } from "../../components/DataTable";
import { ErrorNotice } from "../../components/ErrorNotice";
import { GrantRequired } from "../../components/GrantRequired";
import {
    EmptyState,
    LoadMore,
    LoadingRows,
    PageHeader,
} from "../../components/Page";
import { ResourceLabel } from "../../components/ResourceLabel";
import { WriteError, uncertain } from "../../components/Writes";
import { date, number, words } from "../../format";
import { useSession, useSessionMutation } from "../../queryClient";
import { Badge } from "../../ui/Badge";
import { Button } from "../../ui/Button";
import {
    CreateDestinationDialog,
    EditDestinationDialog,
} from "./DestinationDialog";
import { NotificationNav } from "./NotificationNav";

/** Destinations and the rules that feed them, managed with a grant. */
export function NotificationsPage() {
    const session = useSession();
    const canManage = session !== null && canManageNotifications(session);
    const destinations = useNotificationDestinations(canManage);
    const [creating, setCreating] = useState(false);
    const [editingId, setEditingId] = useState<string | null>(null);
    const [notice, setNotice] = useState<string | null>(null);
    const [testResult, setTestResult] = useState<{
        id: string;
        text: string;
    } | null>(null);
    const items = destinations.data?.pages.flatMap((page) => page.items) ?? [];
    // The list refetches after a conflict, so the dialog reads the revision
    // that a retry has to name rather than the one it opened with.
    const editing = items.find((destination) => destination.id === editingId);

    const test = useSessionMutation<
        {
            destination: Schemas["NotificationDestination"];
            result: Schemas["NotificationDeliveryTest"];
        },
        Schemas["NotificationDestination"]
    >({
        mutationFn: async (destination) => ({
            destination,
            result: await call(
                api.POST("/api/v1/notifications/destinations/{id}/test", {
                    params: {
                        path: { id: destination.id },
                        header: csrfHeader(),
                    },
                }),
            ),
        }),
        onSuccess: ({ destination, result }) => {
            setTestResult({
                id: destination.id,
                text: result.delivered
                    ? `Test delivered (${result.status}).`
                    : `The destination answered ${result.status}.`,
            });
        },
    });

    if (session !== null && !canManage) {
        return (
            <GrantRequired>
                Notifications are organization configuration. Managing
                destinations requires a grant over the whole organization.
            </GrantRequired>
        );
    }

    const columns: DataTableColumn<Schemas["NotificationDestination"]>[] = [
        {
            id: "name",
            header: "Destination",
            cell: (destination) => (
                <ResourceLabel
                    name={destination.name}
                    copy={{ value: destination.id, label: "destination ID" }}
                />
            ),
        },
        {
            id: "kind",
            header: "Kind",
            cell: (destination) => words(destination.kind),
        },
        {
            id: "url",
            header: "URL reference",
            cell: (destination) => (
                <span className="font-mono text-xs">{destination.url_ref}</span>
            ),
        },
        {
            id: "state",
            header: "State",
            cell: (destination) => (
                <Badge>{destination.enabled ? "Enabled" : "Disabled"}</Badge>
            ),
        },
        {
            id: "revision",
            header: "Revision",
            align: "end",
            cell: (destination) => (
                <span className="tabular-nums">
                    {number(destination.revision)}
                </span>
            ),
        },
        {
            id: "updated",
            header: "Updated",
            className: "whitespace-nowrap",
            cell: (destination) => (
                <span className="text-muted-foreground">
                    {date(destination.updated_at)}
                </span>
            ),
        },
        {
            id: "actions",
            header: "",
            align: "end",
            cell: (destination) => (
                <div className="grid justify-items-end gap-1">
                    <div className="flex flex-wrap justify-end gap-1">
                        <Button
                            variant="outline"
                            size="sm"
                            aria-label={`Edit ${destination.name}`}
                            onClick={() => {
                                setNotice(null);
                                setTestResult(null);
                                test.reset();
                                setEditingId(destination.id);
                            }}
                        >
                            Edit
                        </Button>
                        <Button
                            variant="outline"
                            size="sm"
                            aria-label={`Test ${destination.name}`}
                            disabled={test.isPending || uncertain(test.error)}
                            onClick={() => {
                                setTestResult(null);
                                test.reset();
                                test.mutate(destination);
                            }}
                        >
                            {test.isPending &&
                            test.variables.id === destination.id
                                ? "Testing…"
                                : "Test"}
                        </Button>
                    </div>
                    {testResult?.id === destination.id && (
                        <span
                            role="status"
                            className="text-xs text-muted-foreground"
                        >
                            {testResult.text}
                        </span>
                    )}
                    {test.isError && test.variables.id === destination.id && (
                        <WriteError error={test.error} />
                    )}
                </div>
            ),
        },
    ];

    return (
        <section>
            <NotificationNav current="destinations" />
            <PageHeader
                title="Notifications"
                description="Destinations and rules for outbound notifications. A destination says where events are delivered; a rule chooses which events reach it."
                actions={
                    canManage && (
                        <Button
                            size="lg"
                            onClick={() => {
                                setNotice(null);
                                setCreating(true);
                            }}
                        >
                            <PlusIcon aria-hidden="true" />
                            New destination
                        </Button>
                    )
                }
            />
            {notice !== null && (
                <p role="status" className="mb-3 text-sm">
                    {notice}
                </p>
            )}
            {destinations.isPending && (
                <LoadingRows label="Loading destinations…" />
            )}
            {destinations.error && (
                <ErrorNotice
                    error={destinations.error}
                    onRetry={() => {
                        void destinations.refetch();
                    }}
                />
            )}
            {destinations.isSuccess && items.length === 0 && (
                <EmptyState icon={BellRingIcon} title="No destinations yet.">
                    A destination says where notifications are delivered. Create
                    one here, then add a rule that chooses the events it
                    receives.
                </EmptyState>
            )}
            {items.length > 0 && (
                <DataTable
                    rows={items}
                    columns={columns}
                    getRowKey={(destination) => destination.id}
                    caption="Notification destinations"
                />
            )}
            {destinations.isSuccess && (
                <LoadMore
                    shown={items.length}
                    noun="destinations"
                    hasMore={destinations.hasNextPage}
                    loading={destinations.isFetchingNextPage}
                    onLoad={() => {
                        void destinations.fetchNextPage();
                    }}
                />
            )}
            {creating && (
                <CreateDestinationDialog
                    onClose={() => {
                        setCreating(false);
                    }}
                    onCreated={(name) => {
                        setNotice(`Created ${name}.`);
                    }}
                />
            )}
            {editing !== undefined && (
                <EditDestinationDialog
                    key={editing.id}
                    destination={editing}
                    onClose={() => {
                        setEditingId(null);
                    }}
                    onSaved={(name) => {
                        setNotice(`Saved ${name}.`);
                    }}
                />
            )}
        </section>
    );
}
