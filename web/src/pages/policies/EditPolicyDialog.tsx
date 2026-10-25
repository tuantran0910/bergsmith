// The edit dialog: it sends only changed fields plus the revision it read, and
// a stale revision stops the save until the caller reloads the current one.
// Edits stay in the form across a conflict; they are never rebased silently.

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
import { useSessionMutation } from "../../queryClient";
import { Button } from "../../ui/Button";
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogFooter,
    DialogHeader,
    DialogTitle,
} from "../../ui/Dialog";
import { Alert, AlertDescription, AlertTitle } from "../../ui/Alert";
import { WriteError, uncertain } from "../../components/Writes";
import { changed, draftOf, updateBody } from "./draft";
import { PolicyFields } from "./PolicyFields";

export function EditPolicyDialog({
    policy,
    onClose,
    onSaved,
}: {
    /** The row as last read; it updates when the list refetches. */
    policy: Schemas["MaintenancePolicy"];
    onClose: () => void;
    /** Announces the saved policy once the dialog is gone. */
    onSaved: (name: string) => void;
}) {
    const client = useQueryClient();
    const [draft, setDraft] = useState(() => draftOf(policy));
    const [conflicted, setConflicted] = useState(false);
    const body = updateBody(policy, draft);
    const update = useSessionMutation({
        mutationFn: (request: Schemas["MaintenancePolicyUpdate"]) =>
            call(
                api.PATCH("/api/v1/policies/{id}", {
                    params: {
                        path: { id: policy.id },
                        header: csrfHeader(),
                    },
                    body: request,
                }),
            ),
        onSuccess: (updated) => {
            void client.invalidateQueries({ queryKey: ["policies", "list"] });
            onSaved(updated.name);
            onClose();
        },
        onError: (error) => {
            if (isRevisionConflict(error)) {
                setConflicted(true);
                // The row must show the revision the next attempt has to name.
                void client.invalidateQueries({
                    queryKey: ["policies", "list"],
                });
            }
        },
    });
    const blocked = update.isPending || uncertain(update.error);
    const anyChange = changed(policy, draft);
    return (
        <Dialog
            open
            onOpenChange={(open) => {
                if (!open && !update.isPending) onClose();
            }}
        >
            <DialogContent className="sm:max-w-lg">
                <DialogHeader>
                    <DialogTitle className="line-clamp-2" title={policy.name}>
                        Edit {policy.name}
                    </DialogTitle>
                    <DialogDescription>
                        Revision {policy.revision}. Changes apply to future
                        runs; a running job keeps the plan it started with.
                    </DialogDescription>
                </DialogHeader>
                {conflicted && (
                    <Alert role="alert" variant="warning">
                        <TriangleAlertIcon aria-hidden="true" />
                        <AlertTitle>
                            This policy changed while you were editing.
                        </AlertTitle>
                        <AlertDescription>
                            <p>
                                Nothing was saved. Your edits are still here;
                                revision {policy.revision} is current.
                            </p>
                            <Button
                                variant="outline"
                                size="sm"
                                onClick={() => {
                                    setDraft(draftOf(policy));
                                    setConflicted(false);
                                    update.reset();
                                }}
                            >
                                Reload revision {policy.revision}
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
                        <PolicyFields
                            value={draft}
                            onChange={(next) => {
                                setDraft(next);
                                update.reset();
                            }}
                            showEnabled
                            disabled={blocked}
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
                                {update.isPending ? "Saving…" : "Save policy"}
                            </Button>
                        </DialogFooter>
                    </fieldset>
                </form>
                {!conflicted && <WriteError error={update.error} />}
            </DialogContent>
        </Dialog>
    );
}
