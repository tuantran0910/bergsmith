// The create dialog: the planner defaults open the form, and the request names
// every field so the saved policy is the one the form showed.

import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";

import { api, call, csrfHeader, type Schemas } from "../../api/client";
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
import { WriteError, uncertain } from "../../components/Writes";
import { createBody, newDraft } from "./draft";
import { PolicyFields } from "./PolicyFields";

export function CreatePolicyDialog({
    onClose,
    onCreated,
}: {
    onClose: () => void;
    /** Announces the saved policy once the dialog is gone. */
    onCreated: (name: string) => void;
}) {
    const client = useQueryClient();
    const [draft, setDraft] = useState(newDraft);
    const body = createBody(draft);
    const create = useSessionMutation({
        mutationFn: (request: Schemas["MaintenancePolicyCreate"]) =>
            call(
                api.POST("/api/v1/policies", {
                    params: { header: csrfHeader() },
                    body: request,
                }),
            ),
        onSuccess: (created) => {
            void client.invalidateQueries({ queryKey: ["policies", "list"] });
            onCreated(created.name);
            onClose();
        },
    });
    const blocked = create.isPending || uncertain(create.error);
    return (
        <Dialog
            open
            onOpenChange={(open) => {
                if (!open && !create.isPending) onClose();
            }}
        >
            <DialogContent className="sm:max-w-lg">
                <DialogHeader>
                    <DialogTitle>New policy</DialogTitle>
                    <DialogDescription>
                        Reusable settings for scheduled compaction. Schedules
                        bind a policy to a table and a time; changes apply to
                        future runs. The new policy starts enabled.
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
                        <PolicyFields
                            value={draft}
                            onChange={(next) => {
                                setDraft(next);
                                create.reset();
                            }}
                            showEnabled={false}
                            disabled={blocked}
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
                                    : "Create policy"}
                            </Button>
                        </DialogFooter>
                    </fieldset>
                </form>
                <WriteError error={create.error} />
            </DialogContent>
        </Dialog>
    );
}
