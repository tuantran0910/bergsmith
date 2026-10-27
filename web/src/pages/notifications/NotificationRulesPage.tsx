// Notification rules: which events reach a destination, optionally narrowed
// to one connection, table, or worker pool. Edits name the revision each read
// so a concurrent change surfaces instead of being overwritten.

import { BellRingIcon, PlusIcon } from "lucide-react";
import { useState } from "react";

import type { Schemas } from "../../api/client";
import { canManageNotifications } from "../../api/auth";
import {
    useNotificationDestinations,
    useNotificationRules,
} from "../../api/notifications";
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
import { number, shortId } from "../../format";
import { useSession } from "../../queryClient";
import { Badge } from "../../ui/Badge";
import { Button } from "../../ui/Button";
import { LoadMoreOptions } from "./LoadMoreOptions";
import { NotificationNav } from "./NotificationNav";
import { CreateRuleDialog, EditRuleDialog } from "./RuleDialog";
import { eventKindLabel, filterSummary } from "./rules";

/** The rules feeding the organization's notification destinations. */
export function NotificationRulesPage() {
    const session = useSession();
    const canManage = session !== null && canManageNotifications(session);
    const rules = useNotificationRules(canManage);
    const destinations = useNotificationDestinations(canManage);
    const [creating, setCreating] = useState(false);
    const [editingId, setEditingId] = useState<string | null>(null);
    const [notice, setNotice] = useState<string | null>(null);
    const items = rules.data?.pages.flatMap((page) => page.items) ?? [];
    const destinationItems =
        destinations.data?.pages.flatMap((page) => page.items) ?? [];
    // The list refetches after a conflict, so the dialog reads the revision
    // that a retry has to name rather than the one it opened with.
    const editing = items.find((rule) => rule.id === editingId);
    const destinationMore = (
        <LoadMoreOptions
            noun="destinations"
            pending={destinations.isPending}
            loading={destinations.isFetchingNextPage}
            hasMore={destinations.hasNextPage}
            error={destinations.isError ? destinations.error : null}
            onLoad={() => {
                void destinations.fetchNextPage();
            }}
        />
    );

    if (session !== null && !canManage) {
        return (
            <GrantRequired>
                Notifications are organization configuration. Managing rules
                requires a grant over the whole organization.
            </GrantRequired>
        );
    }

    const columns: DataTableColumn<Schemas["NotificationRule"]>[] = [
        {
            id: "name",
            header: "Rule",
            cell: (rule) => (
                <ResourceLabel
                    name={rule.name}
                    copy={{ value: rule.id, label: "rule ID" }}
                />
            ),
        },
        {
            id: "event",
            header: "Event",
            cell: (rule) => eventKindLabel(rule.event_kind),
        },
        {
            id: "destination",
            header: "Destination",
            cell: (rule) =>
                destinationItems.find(
                    (destination) => destination.id === rule.destination_id,
                )?.name ?? `Destination ${shortId(rule.destination_id)}`,
        },
        {
            id: "filter",
            header: "Filter",
            cell: (rule) => filterSummary(rule.filter),
        },
        {
            id: "threshold",
            header: "Threshold",
            align: "end",
            cell: (rule) =>
                rule.repeat_threshold === null
                    ? "-"
                    : number(rule.repeat_threshold),
        },
        {
            id: "state",
            header: "State",
            cell: (rule) => (
                <Badge>{rule.enabled ? "Enabled" : "Disabled"}</Badge>
            ),
        },
        {
            id: "actions",
            header: "",
            align: "end",
            cell: (rule) => (
                <Button
                    variant="outline"
                    size="sm"
                    aria-label={`Edit ${rule.name}`}
                    onClick={() => {
                        setNotice(null);
                        setEditingId(rule.id);
                    }}
                >
                    Edit
                </Button>
            ),
        },
    ];

    return (
        <section>
            <NotificationNav current="rules" />
            <PageHeader
                title="Notification rules"
                description="Rules choose which events reach a destination. Narrow one to a connection, table, or worker pool, and optionally wait for a repeat threshold."
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
                            New rule
                        </Button>
                    )
                }
            />
            {notice !== null && (
                <p role="status" className="mb-3 text-sm">
                    {notice}
                </p>
            )}
            {rules.isPending && <LoadingRows label="Loading rules…" />}
            {rules.error && (
                <ErrorNotice
                    error={rules.error}
                    onRetry={() => {
                        void rules.refetch();
                    }}
                />
            )}
            {rules.isSuccess && items.length === 0 && (
                <EmptyState icon={BellRingIcon} title="No rules yet.">
                    A rule chooses which events reach a destination, such as a
                    failed job or a missed schedule occurrence. Create a
                    destination first, then add a rule here to receive its
                    events.
                </EmptyState>
            )}
            {items.length > 0 && (
                <DataTable
                    rows={items}
                    columns={columns}
                    getRowKey={(rule) => rule.id}
                    caption="Notification rules"
                />
            )}
            {rules.isSuccess && (
                <LoadMore
                    shown={items.length}
                    noun="rules"
                    hasMore={rules.hasNextPage}
                    loading={rules.isFetchingNextPage}
                    onLoad={() => {
                        void rules.fetchNextPage();
                    }}
                />
            )}
            {creating && (
                <CreateRuleDialog
                    destinations={destinationItems}
                    destinationMore={destinationMore}
                    onClose={() => {
                        setCreating(false);
                    }}
                    onCreated={(name) => {
                        setNotice(`Created ${name}.`);
                    }}
                />
            )}
            {editing !== undefined && (
                <EditRuleDialog
                    key={editing.id}
                    rule={editing}
                    destinations={destinationItems}
                    destinationMore={destinationMore}
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
