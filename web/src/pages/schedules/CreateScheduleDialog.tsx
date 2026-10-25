// The create dialog: policy, table, and service identity are chosen once, and
// the timing defaults to a daily run the operator then adjusts.

import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import type { Schemas } from "../../api/client";
import { api, call, csrfHeader } from "../../api/client";
import { usePrincipals } from "../../api/access";
import { useSessionMutation } from "../../queryClient";
import { WriteError, uncertain } from "../../components/Writes";
import { Button } from "../../ui/Button";
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogFooter,
    DialogHeader,
    DialogTitle,
} from "../../ui/Dialog";
import { createBody, newDraft } from "./draft";
import { SchedulePickers, ScheduleTimingFields } from "./ScheduleFields";

export function CreateScheduleDialog({
    onClose,
    onCreated,
    policies,
    tables,
    moreTables,
    loadingTables,
    loadMoreTables,
}: {
    onClose: () => void;
    /** Announces the saved schedule once the dialog is gone. */
    onCreated: (table: string, policy: string) => void;
    policies: readonly { value: string; label: string }[];
    tables: readonly { value: string; label: string }[];
    moreTables: boolean;
    loadingTables: boolean;
    loadMoreTables: () => void;
}) {
    const client = useQueryClient();
    const services = usePrincipals(true);
    const [draft, setDraft] = useState(newDraft);
    const body = createBody(draft);
    const create = useSessionMutation({
        mutationFn: (request: Schemas["MaintenanceScheduleCreate"]) =>
            call(
                api.POST("/api/v1/schedules", {
                    params: { header: csrfHeader() },
                    body: request,
                }),
            ),
        onSuccess: (created) => {
            void client.invalidateQueries({ queryKey: ["schedules", "list"] });
            const policy =
                policies.find((option) => option.value === created.policy_id)
                    ?.label ?? "the policy";
            const table =
                tables.find((option) => option.value === created.table_id)
                    ?.label ?? "the table";
            onCreated(table, policy);
            onClose();
        },
    });
    const blocked = create.isPending || uncertain(create.error);
    const serviceOptions =
        services.data?.pages.flatMap((page) =>
            page.items.map((service) => ({
                value: service.id,
                label: service.display_name,
            })),
        ) ?? [];
    return (
        <Dialog
            open
            onOpenChange={(open) => {
                if (!open && !create.isPending) onClose();
            }}
        >
            <DialogContent className="sm:max-w-xl">
                <DialogHeader>
                    <DialogTitle>New schedule</DialogTitle>
                    <DialogDescription>
                        Binds one policy to one table, at a local time, as a
                        service identity. The policy, table, and identity cannot
                        change later; disable the schedule and create another to
                        move it.
                    </DialogDescription>
                </DialogHeader>
                <form
                    onSubmit={(event) => {
                        event.preventDefault();
                        if (body !== null && !blocked) create.mutate(body);
                    }}
                >
                    <fieldset
                        disabled={blocked}
                        className="m-0 grid gap-4 border-0 p-0"
                    >
                        <SchedulePickers
                            draft={draft}
                            onChange={(next) => {
                                setDraft(next);
                                create.reset();
                            }}
                            disabled={blocked}
                            policies={policies}
                            tables={tables}
                            services={serviceOptions}
                            moreTables={moreTables}
                            loadingTables={loadingTables}
                            onLoadTables={loadMoreTables}
                        />
                        <ScheduleTimingFields
                            draft={draft}
                            onChange={(next) => {
                                setDraft(next);
                                create.reset();
                            }}
                            disabled={blocked}
                            showEnabled={false}
                        />
                        <DialogFooter>
                            <Button
                                type="button"
                                variant="outline"
                                disabled={create.isPending}
                                onClick={onClose}
                            >
                                Cancel
                            </Button>
                            <Button
                                type="submit"
                                disabled={body === null || blocked}
                            >
                                {create.isPending
                                    ? "Creating…"
                                    : "Create schedule"}
                            </Button>
                        </DialogFooter>
                    </fieldset>
                </form>
                {services.error !== null && (
                    <WriteError error={services.error} />
                )}
                <WriteError error={create.error} />
            </DialogContent>
        </Dialog>
    );
}
