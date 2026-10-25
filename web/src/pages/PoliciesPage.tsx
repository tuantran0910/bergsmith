// Maintenance policies: what a scheduled run may rewrite. The page lists the
// organization's configuration and edits it with the revision each read, so a
// concurrent change surfaces instead of being overwritten.

import { useState } from "react";
import { PlusIcon, SlidersHorizontalIcon } from "lucide-react";

import type { Schemas } from "../api/client";
import { canManageSchedules } from "../api/auth";
import { usePolicies } from "../api/policies";
import { DataTable, type DataTableColumn } from "../components/DataTable";
import { ErrorNotice } from "../components/ErrorNotice";
import { GrantRequired } from "../components/GrantRequired";
import {
    EmptyState,
    LoadMore,
    LoadingRows,
    PageHeader,
} from "../components/Page";
import { ResourceLabel } from "../components/ResourceLabel";
import { bytes, date, number } from "../format";
import { useSession } from "../queryClient";
import { Badge } from "../ui/Badge";
import { Button } from "../ui/Button";
import { CreatePolicyDialog } from "./policies/CreatePolicyDialog";
import { EditPolicyDialog } from "./policies/EditPolicyDialog";

/** Reusable compaction settings, managed with organization-wide grants. */
export function PoliciesPage() {
    const session = useSession();
    const canManage = session !== null && canManageSchedules(session);
    const policies = usePolicies(canManage);
    const [creating, setCreating] = useState(false);
    const [editingId, setEditingId] = useState<string | null>(null);
    const [notice, setNotice] = useState<string | null>(null);
    const items = policies.data?.pages.flatMap((page) => page.items) ?? [];
    // The list refetches after a conflict, so the dialog reads the revision
    // that a retry has to name rather than the one it opened with.
    const editing = items.find((policy) => policy.id === editingId);

    if (session !== null && !canManage) {
        return (
            <GrantRequired>
                Policies are organization configuration. Managing them requires
                a grant over the whole organization.
            </GrantRequired>
        );
    }

    const columns: DataTableColumn<Schemas["MaintenancePolicy"]>[] = [
        {
            id: "name",
            header: "Policy",
            cell: (policy) => (
                <ResourceLabel
                    name={policy.name}
                    copy={{ value: policy.id, label: "policy ID" }}
                />
            ),
        },
        {
            id: "operation",
            header: "Operation",
            cell: () => "Binpack compaction",
        },
        {
            id: "target",
            header: "Target file size",
            align: "end",
            cell: (policy) => (
                <span className="tabular-nums">
                    {bytes(policy.target_file_size_bytes)}
                </span>
            ),
        },
        {
            id: "minimum",
            header: "Minimum input files",
            align: "end",
            cell: (policy) => (
                <span className="tabular-nums">
                    {number(policy.min_input_files)}
                </span>
            ),
        },
        {
            id: "budget",
            header: "Run budget",
            align: "end",
            cell: (policy) => (
                <span className="tabular-nums">
                    {bytes(policy.max_input_bytes)}
                </span>
            ),
        },
        {
            id: "state",
            header: "State",
            cell: (policy) => (
                <Badge>{policy.enabled ? "Enabled" : "Disabled"}</Badge>
            ),
        },
        {
            id: "revision",
            header: "Revision",
            align: "end",
            cell: (policy) => (
                <span className="tabular-nums">{number(policy.revision)}</span>
            ),
        },
        {
            id: "updated",
            header: "Updated",
            className: "whitespace-nowrap",
            cell: (policy) => (
                <span className="text-muted-foreground">
                    {date(policy.updated_at)}
                </span>
            ),
        },
        {
            id: "actions",
            header: "",
            align: "end",
            cell: (policy) => (
                <Button
                    variant="outline"
                    size="sm"
                    aria-label={`Edit ${policy.name}`}
                    onClick={() => {
                        setNotice(null);
                        setEditingId(policy.id);
                    }}
                >
                    Edit
                </Button>
            ),
        },
    ];

    return (
        <section>
            <PageHeader
                title="Policies"
                description="Reusable compaction settings. A schedule binds one policy to one table and a time, and applies policy changes to future runs."
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
                            New policy
                        </Button>
                    )
                }
            />
            {notice !== null && (
                <p role="status" className="mb-3 text-sm">
                    {notice}
                </p>
            )}
            {policies.isPending && <LoadingRows label="Loading policies…" />}
            {policies.error && (
                <ErrorNotice
                    error={policies.error}
                    onRetry={() => {
                        void policies.refetch();
                    }}
                />
            )}
            {policies.isSuccess && items.length === 0 && (
                <EmptyState
                    icon={SlidersHorizontalIcon}
                    title="No policies yet."
                >
                    A policy says what a scheduled run may rewrite. Create one
                    here, then bind it to a table with a schedule.
                </EmptyState>
            )}
            {items.length > 0 && (
                <DataTable
                    rows={items}
                    columns={columns}
                    getRowKey={(policy) => policy.id}
                    caption="Maintenance policies"
                />
            )}
            {policies.isSuccess && (
                <LoadMore
                    shown={items.length}
                    noun="policies"
                    hasMore={policies.hasNextPage}
                    loading={policies.isFetchingNextPage}
                    onLoad={() => {
                        void policies.fetchNextPage();
                    }}
                />
            )}
            {creating && (
                <CreatePolicyDialog
                    onClose={() => {
                        setCreating(false);
                    }}
                    onCreated={(name) => {
                        setNotice(`Created ${name}.`);
                    }}
                />
            )}
            {editing !== undefined && (
                <EditPolicyDialog
                    key={editing.id}
                    policy={editing}
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
