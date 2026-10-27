// Destinations are organization configuration edited against the revision
// each read. The create dialog names every field; the edit dialog starts from
// the stored row, sends only what changed, and never hides a conflict.

import { useQueryClient } from "@tanstack/react-query";
import { TriangleAlertIcon } from "lucide-react";
import { useId, useState, type ReactNode } from "react";

import {
    api,
    call,
    csrfHeader,
    isRevisionConflict,
    type Schemas,
} from "../../api/client";
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
import { Input, Label } from "../../ui/Input";
import { Select } from "../../ui/Select";
import { WriteError, uncertain } from "../../components/Writes";

type DestinationKind = Schemas["NotificationDestination"]["kind"];

/** One destination's fields while the form holds them. */
interface DestinationDraft {
    name: string;
    kind: DestinationKind;
    url_ref: string;
    signing_secret_ref: string;
    enabled: boolean;
}

const KINDS: readonly { value: DestinationKind; label: string }[] = [
    { value: "webhook", label: "Webhook" },
    { value: "slack", label: "Slack" },
];

function newDraft(): DestinationDraft {
    return {
        name: "",
        kind: "webhook",
        url_ref: "",
        signing_secret_ref: "",
        enabled: true,
    };
}

function draftOf(
    destination: Schemas["NotificationDestination"],
): DestinationDraft {
    return {
        name: destination.name,
        kind: destination.kind,
        url_ref: destination.url_ref,
        signing_secret_ref: destination.signing_secret_ref ?? "",
        enabled: destination.enabled,
    };
}

function createBody(
    draft: DestinationDraft,
): Schemas["CreateNotificationDestination"] | null {
    const name = draft.name.trim();
    const url = draft.url_ref.trim();
    const secret = draft.signing_secret_ref.trim();
    if (
        name === "" ||
        url === "" ||
        (draft.kind === "webhook" && secret === "")
    ) {
        return null;
    }
    const body: Schemas["CreateNotificationDestination"] = {
        name,
        kind: draft.kind,
        url_ref: url,
        enabled: true,
    };
    if (draft.kind === "webhook") body.signing_secret_ref = secret;
    return body;
}

function updateBody(
    destination: Schemas["NotificationDestination"],
    draft: DestinationDraft,
): Schemas["UpdateNotificationDestination"] | null {
    const name = draft.name.trim();
    const url = draft.url_ref.trim();
    const secret = draft.signing_secret_ref.trim();
    if (
        name === "" ||
        url === "" ||
        (destination.kind === "webhook" && secret === "")
    ) {
        return null;
    }
    const body: Schemas["UpdateNotificationDestination"] = {
        revision: destination.revision,
    };
    if (name !== destination.name) body.name = name;
    if (url !== destination.url_ref) body.url_ref = url;
    if (
        destination.kind === "webhook" &&
        secret !== (destination.signing_secret_ref ?? "")
    ) {
        body.signing_secret_ref = secret;
    }
    if (draft.enabled !== destination.enabled) body.enabled = draft.enabled;
    return body;
}

function changed(
    destination: Schemas["NotificationDestination"],
    draft: DestinationDraft,
): boolean {
    const body = updateBody(destination, draft);
    return (
        body !== null && Object.keys(body).some((field) => field !== "revision")
    );
}

/** A labelled control with an optional hint read as its description. */
function Field({
    id,
    label,
    hint,
    children,
}: {
    id: string;
    label: string;
    hint?: string;
    children: ReactNode;
}) {
    return (
        <div className="grid content-start gap-1.5">
            <Label htmlFor={id}>{label}</Label>
            {children}
            {hint !== undefined && (
                <span
                    id={`${id}-hint`}
                    className="text-xs text-muted-foreground"
                >
                    {hint}
                </span>
            )}
        </div>
    );
}

function DestinationFields({
    value,
    onChange,
    showEnabled,
    lockedKind,
    disabled,
}: {
    value: DestinationDraft;
    onChange: (value: DestinationDraft) => void;
    /** Edit offers the enabled flag; a created destination starts enabled. */
    showEnabled: boolean;
    /** The kind is chosen at creation and cannot change afterwards. */
    lockedKind: boolean;
    disabled: boolean;
}) {
    const prefix = useId();
    const name = `${prefix}-name`;
    const kind = `${prefix}-kind`;
    const url = `${prefix}-url`;
    const secret = `${prefix}-secret`;
    return (
        <div className="grid gap-4">
            <Field
                id={name}
                label="Destination name"
                hint="Letters, digits, spaces, dots, underscores, and hyphens, starting with a letter or digit."
            >
                <Input
                    id={name}
                    aria-describedby={`${name}-hint`}
                    required
                    maxLength={200}
                    pattern={"[A-Za-z0-9][A-Za-z0-9 ._\\-]*"}
                    title="Letters, digits, spaces, dots, underscores, and hyphens, starting with a letter or digit"
                    value={value.name}
                    onChange={(event) => {
                        onChange({ ...value, name: event.target.value });
                    }}
                    autoComplete="off"
                    disabled={disabled}
                />
            </Field>
            {lockedKind ? (
                <div className="grid gap-1.5 text-sm">
                    <span className="font-medium">Kind</span>
                    <p className="m-0 text-muted-foreground">
                        {value.kind === "webhook" ? "Webhook" : "Slack"}
                    </p>
                    <span className="text-xs text-muted-foreground">
                        A destination's kind cannot change.
                    </span>
                </div>
            ) : (
                <Field id={kind} label="Kind">
                    <Select
                        id={kind}
                        value={value.kind}
                        onValueChange={(next) => {
                            onChange({ ...value, kind: next });
                        }}
                        options={KINDS}
                    />
                </Field>
            )}
            <Field
                id={url}
                label="URL reference"
                hint="The name of a file under notifications.secrets_dir. Never a URL; the coordinator reads the address from that file."
            >
                <Input
                    id={url}
                    aria-describedby={`${url}-hint`}
                    required
                    value={value.url_ref}
                    onChange={(event) => {
                        onChange({ ...value, url_ref: event.target.value });
                    }}
                    autoComplete="off"
                    disabled={disabled}
                />
            </Field>
            {value.kind === "webhook" && (
                <Field
                    id={secret}
                    label="Signing secret reference"
                    hint="The name of a file under notifications.secrets_dir. Never the secret itself."
                >
                    <Input
                        id={secret}
                        aria-describedby={`${secret}-hint`}
                        required
                        value={value.signing_secret_ref}
                        onChange={(event) => {
                            onChange({
                                ...value,
                                signing_secret_ref: event.target.value,
                            });
                        }}
                        autoComplete="off"
                        disabled={disabled}
                    />
                </Field>
            )}
            {showEnabled && (
                <label className="flex items-center gap-2 text-sm">
                    <input
                        type="checkbox"
                        checked={value.enabled}
                        onChange={(event) => {
                            onChange({
                                ...value,
                                enabled: event.target.checked,
                            });
                        }}
                        disabled={disabled}
                    />
                    Enabled for notifications
                </label>
            )}
        </div>
    );
}

/** Creates a destination; it starts enabled and the kind is chosen here. */
export function CreateDestinationDialog({
    onClose,
    onCreated,
}: {
    onClose: () => void;
    /** Announces the saved destination once the dialog is gone. */
    onCreated: (name: string) => void;
}) {
    const client = useQueryClient();
    const [draft, setDraft] = useState(newDraft);
    const body = createBody(draft);
    const create = useSessionMutation({
        mutationFn: (request: Schemas["CreateNotificationDestination"]) =>
            call(
                api.POST("/api/v1/notifications/destinations", {
                    params: { header: csrfHeader() },
                    body: request,
                }),
            ),
        onSuccess: (created) => {
            void client.invalidateQueries({
                queryKey: ["notifications", "destinations"],
            });
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
                    <DialogTitle>New destination</DialogTitle>
                    <DialogDescription>
                        Where notifications are delivered. The URL and signing
                        secret are file references read by the coordinator, not
                        addresses or secrets typed here. The new destination
                        starts enabled.
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
                        <DestinationFields
                            value={draft}
                            onChange={(next) => {
                                setDraft(next);
                                create.reset();
                            }}
                            showEnabled={false}
                            lockedKind={false}
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
                                    : "Create destination"}
                            </Button>
                        </DialogFooter>
                    </fieldset>
                </form>
                <WriteError error={create.error} />
            </DialogContent>
        </Dialog>
    );
}

/** Edits one destination, naming the revision the row showed. */
export function EditDestinationDialog({
    destination,
    onClose,
    onSaved,
}: {
    /** The row as last read; it updates when the list refetches. */
    destination: Schemas["NotificationDestination"];
    onClose: () => void;
    /** Announces the saved destination once the dialog is gone. */
    onSaved: (name: string) => void;
}) {
    const client = useQueryClient();
    // The baseline is the row this edit opened. Refreshing props must not
    // silently move the revision under an unchanged draft: only an explicit
    // reload adopts the newer row.
    const [baseline, setBaseline] = useState(destination);
    const [draft, setDraft] = useState(() => draftOf(destination));
    const [conflicted, setConflicted] = useState(false);
    const body = updateBody(baseline, draft);
    const update = useSessionMutation({
        mutationFn: (request: Schemas["UpdateNotificationDestination"]) =>
            call(
                api.PATCH("/api/v1/notifications/destinations/{id}", {
                    params: {
                        path: { id: baseline.id },
                        header: csrfHeader(),
                    },
                    body: request,
                }),
            ),
        onSuccess: (updated) => {
            void client.invalidateQueries({
                queryKey: ["notifications", "destinations"],
            });
            onSaved(updated.name);
            onClose();
        },
        onError: (error) => {
            if (isRevisionConflict(error)) {
                setConflicted(true);
                // The row must show the revision the next attempt names.
                void client.invalidateQueries({
                    queryKey: ["notifications", "destinations"],
                });
            }
        },
    });
    const blocked = update.isPending || uncertain(update.error);
    const anyChange = changed(baseline, draft);
    // Invalidation refreshes the list; only a changed revision can be adopted.
    function reload() {
        if (destination.revision === baseline.revision) {
            void client.invalidateQueries({
                queryKey: ["notifications", "destinations"],
            });
            return;
        }
        setBaseline(destination);
        setDraft(draftOf(destination));
        setConflicted(false);
        update.reset();
    }
    return (
        <Dialog
            open
            onOpenChange={(open) => {
                if (!open && !update.isPending) onClose();
            }}
        >
            <DialogContent className="sm:max-w-lg">
                <DialogHeader>
                    <DialogTitle className="line-clamp-2" title={baseline.name}>
                        Edit {baseline.name}
                    </DialogTitle>
                    <DialogDescription>
                        Revision {baseline.revision}. Changes apply to future
                        notifications.
                    </DialogDescription>
                </DialogHeader>
                {conflicted && (
                    <Alert role="alert" variant="warning">
                        <TriangleAlertIcon aria-hidden="true" />
                        <AlertTitle>
                            This destination changed while you were editing.
                        </AlertTitle>
                        <AlertDescription>
                            <p>
                                Nothing was saved. Your edits are still here;
                                revision {destination.revision} is current.
                            </p>
                            <Button
                                variant="outline"
                                size="sm"
                                onClick={reload}
                            >
                                Reload revision {destination.revision}
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
                        <DestinationFields
                            value={draft}
                            onChange={(next) => {
                                setDraft(next);
                                update.reset();
                            }}
                            showEnabled
                            lockedKind
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
                                {update.isPending
                                    ? "Saving…"
                                    : "Save destination"}
                            </Button>
                        </DialogFooter>
                    </fieldset>
                </form>
                <WriteError error={update.error} />
            </DialogContent>
        </Dialog>
    );
}
