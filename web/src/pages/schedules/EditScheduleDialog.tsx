// The edit dialog: timing and the enabled flag, against the revision it read.
// The bound policy, table, and service identity are shown but cannot change.
// A stale revision keeps the edits and stops the save until it is reloaded.

import { useQueryClient } from "@tanstack/react-query";
import { TriangleAlertIcon } from "lucide-react";
import { useState } from "react";

import {
    api,
    call,
    csrfHeader,
    isRevisionConflict,
    type Schemas,
} from "../../api/client";
import { changed, draftOf, updateBody } from "./draft";
import { WriteError, uncertain } from "../../components/Writes";
import { useSessionMutation } from "../../queryClient";
import { Alert, AlertDescription, AlertTitle } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogFooter,
    DialogHeader,
    DialogTitle,
} from "../../ui/Dialog";
import { ScheduleTimingFields } from "./ScheduleFields";

export function EditScheduleDialog({
    schedule,
    policy,
    table,
    onClose,
    onSaved,
}: {
    /** The schedule as last read; it updates when the page refetches. */
    schedule: Schemas["MaintenanceSchedule"];
    policy: string;
    table: string;
    onClose: () => void;
    /** Announces the saved schedule once the dialog is gone. */
    onSaved: (label: string) => void;
}) {
    const client = useQueryClient();
    const [draft, setDraft] = useState(() => draftOf(schedule));
    const [conflicted, setConflicted] = useState(false);
    const body = updateBody(schedule, draft);
    const update = useSessionMutation({
        mutationFn: (request: Schemas["MaintenanceScheduleUpdate"]) =>
            call(
                api.PATCH("/api/v1/schedules/{id}", {
                    params: {
                        path: { id: schedule.id },
                        header: csrfHeader(),
                    },
                    body: request,
                }),
            ),
        onSuccess: () => {
            void client.invalidateQueries({ queryKey: ["schedules", "list"] });
            void client.invalidateQueries({
                queryKey: ["schedule", schedule.id],
            });
            onSaved(table);
            onClose();
        },
        onError: (error) => {
            if (isRevisionConflict(error)) {
                setConflicted(true);
                // The page must show the revision the next attempt has to name.
                void client.invalidateQueries({
                    queryKey: ["schedules", "list"],
                });
                void client.invalidateQueries({
                    queryKey: ["schedule", schedule.id],
                });
            }
        },
    });
    const blocked = update.isPending || uncertain(update.error);
    const anyChange = changed(schedule, draft);
    return (
        <Dialog
            open
            onOpenChange={(open) => {
                if (!open && !update.isPending) onClose();
            }}
        >
            <DialogContent className="sm:max-w-xl">
                <DialogHeader>
                    <DialogTitle className="line-clamp-2" title={table}>
                        Edit schedule for {table}
                    </DialogTitle>
                    <DialogDescription>
                        Revision {schedule.revision}. The policy, table, and
                        service identity cannot change. Timing changes apply to
                        future evaluations; a running job keeps its plan.
                    </DialogDescription>
                </DialogHeader>
                <dl className="m-0 grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1 text-sm">
                    <dt className="text-muted-foreground">Policy</dt>
                    <dd className="m-0">{policy}</dd>
                    <dt className="text-muted-foreground">Service identity</dt>
                    <dd className="m-0">{schedule.service_identity_name}</dd>
                </dl>
                {conflicted && (
                    <Alert role="alert" variant="warning">
                        <TriangleAlertIcon aria-hidden="true" />
                        <AlertTitle>
                            This schedule changed while you were editing.
                        </AlertTitle>
                        <AlertDescription>
                            <p>
                                Nothing was saved. Your edits are still here;
                                revision {schedule.revision} is current.
                            </p>
                            <Button
                                variant="outline"
                                size="sm"
                                onClick={() => {
                                    setDraft(draftOf(schedule));
                                    setConflicted(false);
                                    update.reset();
                                }}
                            >
                                Reload revision {schedule.revision}
                            </Button>
                        </AlertDescription>
                    </Alert>
                )}
                <form
                    onSubmit={(event) => {
                        event.preventDefault();
                        if (
                            body !== null &&
                            anyChange &&
                            !blocked &&
                            !conflicted
                        ) {
                            update.mutate(body);
                        }
                    }}
                >
                    <fieldset
                        disabled={blocked}
                        className="m-0 grid gap-4 border-0 p-0"
                    >
                        <ScheduleTimingFields
                            draft={draft}
                            onChange={(next) => {
                                setDraft(next);
                                update.reset();
                            }}
                            disabled={blocked}
                            showEnabled
                        />
                        <DialogFooter>
                            <Button
                                type="button"
                                variant="outline"
                                disabled={update.isPending}
                                onClick={onClose}
                            >
                                Cancel
                            </Button>
                            <Button
                                type="submit"
                                disabled={
                                    body === null ||
                                    !anyChange ||
                                    blocked ||
                                    conflicted
                                }
                            >
                                {update.isPending ? "Saving…" : "Save schedule"}
                            </Button>
                        </DialogFooter>
                    </fieldset>
                </form>
                {!conflicted && <WriteError error={update.error} />}
            </DialogContent>
        </Dialog>
    );
}
