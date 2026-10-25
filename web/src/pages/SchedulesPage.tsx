// Schedules: which policy runs against which table, when, and as whom. The list
// leads with the table, because that is what an operator recognises, and shows
// the next instant the coordinator computed for each schedule.

import { useState } from "react";
import { Link } from "@tanstack/react-router";
import { CalendarClockIcon, PlusIcon } from "lucide-react";

import {
    canManageSchedules,
    canManageWorkers,
    hasOrganizationPermission,
} from "../api/auth";
import type { Schemas } from "../api/client";
import { useSchedules } from "../api/schedules";
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
import { number, timestamp } from "../format";
import { useSession } from "../queryClient";
import { Badge } from "../ui/Badge";
import { Button } from "../ui/Button";
import { CreateScheduleDialog } from "./schedules/CreateScheduleDialog";
import { clock } from "./schedules/draft";
import { useScheduleLabels } from "./schedules/labels";

/** The window in the schedule's local time, or `Any time`. */
function windowText(window: Schemas["MaintenanceWindow"] | null): string {
    return window === null
        ? "Any time"
        : `${clock(window.start_minute)}–${clock(window.end_minute)}`;
}

/** Organization schedules, shown to organization-wide administrators. */
export function SchedulesPage() {
    const session = useSession();
    const canManage = session !== null && canManageSchedules(session);
    const schedules = useSchedules(canManage);
    const labels = useScheduleLabels(canManage);
    const [creating, setCreating] = useState(false);
    const [notice, setNotice] = useState<string | null>(null);
    const items = schedules.data?.pages.flatMap((page) => page.items) ?? [];
    const canPick =
        session !== null &&
        hasOrganizationPermission(session, "inventory:read") &&
        canManageWorkers(session);

    if (session !== null && !canManage) {
        return (
            <GrantRequired>
                Schedules are organization configuration. Managing them requires
                a grant over the whole organization.
            </GrantRequired>
        );
    }

    const columns: DataTableColumn<Schemas["MaintenanceSchedule"]>[] = [
        {
            id: "table",
            header: "Table",
            cell: (schedule) => (
                <ResourceLabel
                    name={
                        <Link
                            to="/schedules/$scheduleId"
                            params={{ scheduleId: schedule.id }}
                            title={schedule.id}
                            className="break-words"
                        >
                            {labels.tableName(schedule.table_id)}
                        </Link>
                    }
                    context={labels.policyName(schedule.policy_id)}
                    copy={{ value: schedule.id, label: "schedule ID" }}
                />
            ),
        },
        {
            id: "timing",
            header: "Runs",
            cell: (schedule) => (
                <span className="grid gap-0.5">
                    <code className="text-xs">{schedule.cron_expression}</code>
                    <span className="text-xs text-muted-foreground">
                        {schedule.time_zone} · {windowText(schedule.window)}
                    </span>
                </span>
            ),
        },
        {
            id: "identity",
            header: "Service identity",
            cell: (schedule) => (
                <span className="text-muted-foreground">
                    {schedule.service_identity_name}
                </span>
            ),
        },
        {
            id: "state",
            header: "State",
            cell: (schedule) => (
                <Badge>{schedule.enabled ? "Enabled" : "Disabled"}</Badge>
            ),
        },
        {
            id: "next",
            header: "Next",
            className: "whitespace-nowrap",
            cell: (schedule) => {
                const next = schedule.next_occurrences[0];
                return (
                    <span className="text-muted-foreground">
                        {next === undefined ? "None" : timestamp(next)}
                    </span>
                );
            },
        },
        {
            id: "revision",
            header: "Revision",
            align: "end",
            cell: (schedule) => (
                <span className="tabular-nums">
                    {number(schedule.revision)}
                </span>
            ),
        },
        {
            id: "actions",
            header: "",
            align: "end",
            cell: (schedule) => (
                <Link
                    to="/schedules/$scheduleId"
                    params={{ scheduleId: schedule.id }}
                    aria-label={`Open the schedule for ${labels.tableName(schedule.table_id)}`}
                    className="text-sm font-medium text-primary underline-offset-4 hover:underline"
                >
                    Open
                </Link>
            ),
        },
    ];

    return (
        <section>
            <PageHeader
                title="Schedules"
                description="Binds one policy to one table at a local time, as a service identity."
                actions={
                    canManage &&
                    canPick && (
                        <Button
                            size="lg"
                            onClick={() => {
                                setNotice(null);
                                setCreating(true);
                            }}
                        >
                            <PlusIcon aria-hidden="true" />
                            New schedule
                        </Button>
                    )
                }
            />
            {canManage && !canPick && (
                <p className="mb-3 text-sm text-muted-foreground">
                    Creating a schedule also needs organization-wide{" "}
                    <code>inventory:read</code> to choose a table and{" "}
                    <code>workers:manage</code> to choose a service identity.
                </p>
            )}
            {notice !== null && (
                <p role="status" className="mb-3 text-sm">
                    {notice}
                </p>
            )}
            {schedules.isPending && <LoadingRows label="Loading schedules…" />}
            {schedules.error && (
                <ErrorNotice
                    error={schedules.error}
                    onRetry={() => {
                        void schedules.refetch();
                    }}
                />
            )}
            {schedules.isSuccess && items.length === 0 && (
                <EmptyState icon={CalendarClockIcon} title="No schedules yet.">
                    A schedule runs one policy against one table at a local
                    time. Create one here; the coordinator computes its next
                    evaluations.
                </EmptyState>
            )}
            {items.length > 0 && (
                <DataTable
                    rows={items}
                    columns={columns}
                    getRowKey={(schedule) => schedule.id}
                    caption="Maintenance schedules"
                />
            )}
            {schedules.isSuccess && (
                <LoadMore
                    shown={items.length}
                    noun="schedules"
                    hasMore={schedules.hasNextPage}
                    loading={schedules.isFetchingNextPage}
                    onLoad={() => {
                        void schedules.fetchNextPage();
                    }}
                />
            )}
            {creating && (
                <CreateScheduleDialog
                    onClose={() => {
                        setCreating(false);
                    }}
                    onCreated={(table, policy) => {
                        setNotice(
                            `Created the schedule for ${table} (${policy}).`,
                        );
                    }}
                    policies={labels.policies.map((policy) => ({
                        value: policy.id,
                        label: policy.enabled
                            ? policy.name
                            : `${policy.name} (disabled)`,
                    }))}
                    tables={labels.tables.map((table) => ({
                        value: table.id,
                        label: labels.tableName(table.id),
                    }))}
                    moreTables={labels.moreTables}
                    loadingTables={labels.loadingTables}
                    loadMoreTables={labels.loadMoreTables}
                />
            )}
        </section>
    );
}
