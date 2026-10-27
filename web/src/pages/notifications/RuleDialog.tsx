// Rules choose which events reach a destination. The create dialog picks the
// event kind; the edit dialog keeps it fixed, names the revision it read, and
// sends every editable field so the saved rule is the one the form showed.

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
import { Input, Label } from "../../ui/Input";
import { Select } from "../../ui/Select";
import {
    EVENT_KINDS,
    FILTER_MODES,
    createRuleBody,
    eventKindLabel,
    isRecurring,
    newRuleDraft,
    ruleDraftOf,
    updateRuleBody,
    type RuleDraft,
} from "./rules";

/** A labelled control with an optional hint read as its description. */
function Field({
    id,
    label,
    hint,
    children,
}: {
    id: string;
    label: string;
    hint?: string | undefined;
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

function RuleFields({
    value,
    onChange,
    destinations,
    destinationMore,
    lockedEventKind,
    showEnabled,
    disabled,
}: {
    value: RuleDraft;
    onChange: (value: RuleDraft) => void;
    destinations: readonly Schemas["NotificationDestination"][];
    /** A paginated list's next-page control, when it has one. */
    destinationMore?: ReactNode;
    /** Edit shows the stored event kind; it cannot be changed. */
    lockedEventKind: boolean;
    /** Edit offers the enabled flag; a created rule starts enabled. */
    showEnabled: boolean;
    disabled: boolean;
}) {
    const prefix = useId();
    const name = `${prefix}-name`;
    const destination = `${prefix}-destination`;
    const event = `${prefix}-event`;
    const filter = `${prefix}-filter`;
    const filterValue = `${prefix}-filter-value`;
    const threshold = `${prefix}-threshold`;
    const options = destinations.map((item) => ({
        value: item.id,
        label: item.name,
    }));
    const valueHint =
        value.filter_mode === "connection"
            ? "The connection ID the rule watches."
            : value.filter_mode === "table"
              ? "The table ID the rule watches."
              : "The worker pool the rule watches.";
    return (
        <div className="grid gap-4">
            <Field
                id={name}
                label="Rule name"
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
                    onChange={(input) => {
                        onChange({ ...value, name: input.target.value });
                    }}
                    autoComplete="off"
                    disabled={disabled}
                />
            </Field>
            <Field
                id={destination}
                label="Destination"
                hint={
                    options.length === 0
                        ? "Create a destination before adding a rule."
                        : undefined
                }
            >
                <Select
                    id={destination}
                    value={value.destination_id}
                    onValueChange={(next) => {
                        onChange({ ...value, destination_id: next });
                    }}
                    options={options}
                />
                {destinationMore}
            </Field>
            {lockedEventKind ? (
                <div className="grid gap-1.5 text-sm">
                    <span className="font-medium">Event</span>
                    <p className="m-0 text-muted-foreground">
                        {eventKindLabel(value.event_kind)}
                    </p>
                    <span className="text-xs text-muted-foreground">
                        A rule's event kind cannot change.
                    </span>
                </div>
            ) : (
                <Field id={event} label="Event">
                    <Select
                        id={event}
                        value={value.event_kind}
                        onValueChange={(next) => {
                            onChange({
                                ...value,
                                event_kind: next,
                                repeat_threshold: isRecurring(next)
                                    ? value.repeat_threshold
                                    : "",
                            });
                        }}
                        options={EVENT_KINDS}
                    />
                </Field>
            )}
            <Field
                id={filter}
                label="Filter"
                hint="Narrow the rule to one connection, one table, or one worker pool. Any event kind may match."
            >
                <Select
                    id={filter}
                    value={value.filter_mode}
                    onValueChange={(next) => {
                        onChange({
                            ...value,
                            filter_mode: next,
                            filter_value:
                                next === "none" ? "" : value.filter_value,
                        });
                    }}
                    options={FILTER_MODES}
                />
            </Field>
            {value.filter_mode !== "none" && (
                <Field id={filterValue} label="Filter value" hint={valueHint}>
                    <Input
                        id={filterValue}
                        aria-describedby={`${filterValue}-hint`}
                        required
                        value={value.filter_value}
                        onChange={(input) => {
                            onChange({
                                ...value,
                                filter_value: input.target.value,
                            });
                        }}
                        autoComplete="off"
                        disabled={disabled}
                    />
                </Field>
            )}
            {isRecurring(value.event_kind) && (
                <Field
                    id={threshold}
                    label="Repeat threshold"
                    hint="Send only after this many repetitions. Leave empty to send every occurrence."
                >
                    <Input
                        id={threshold}
                        aria-describedby={`${threshold}-hint`}
                        type="number"
                        inputMode="numeric"
                        min={1}
                        step={1}
                        value={value.repeat_threshold}
                        onChange={(input) => {
                            onChange({
                                ...value,
                                repeat_threshold: input.target.value,
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
                        onChange={(input) => {
                            onChange({
                                ...value,
                                enabled: input.target.checked,
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

/** Creates a rule for one destination; it starts enabled. */
export function CreateRuleDialog({
    destinations,
    destinationMore,
    onClose,
    onCreated,
}: {
    destinations: readonly Schemas["NotificationDestination"][];
    destinationMore?: ReactNode;
    onClose: () => void;
    /** Announces the saved rule once the dialog is gone. */
    onCreated: (name: string) => void;
}) {
    const client = useQueryClient();
    const [draft, setDraft] = useState(() => newRuleDraft(destinations));
    // The destination list may arrive after the dialog opens; bind the first
    // one then rather than leaving the form unable to submit.
    const value =
        draft.destination_id === "" && destinations[0] !== undefined
            ? { ...draft, destination_id: destinations[0].id }
            : draft;
    const body = createRuleBody(value);
    const create = useSessionMutation({
        mutationFn: (request: Schemas["CreateNotificationRule"]) =>
            call(
                api.POST("/api/v1/notifications/rules", {
                    params: { header: csrfHeader() },
                    body: request,
                }),
            ),
        onSuccess: (created) => {
            void client.invalidateQueries({
                queryKey: ["notifications", "rules"],
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
                    <DialogTitle>New rule</DialogTitle>
                    <DialogDescription>
                        A rule chooses which events reach a destination. The new
                        rule starts enabled.
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
                        <RuleFields
                            value={value}
                            onChange={(next) => {
                                setDraft(next);
                                create.reset();
                            }}
                            destinations={destinations}
                            destinationMore={destinationMore}
                            lockedEventKind={false}
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
                                {create.isPending ? "Creating…" : "Create rule"}
                            </Button>
                        </DialogFooter>
                    </fieldset>
                </form>
                <WriteError error={create.error} />
            </DialogContent>
        </Dialog>
    );
}

/** Edits one rule, keeping its event kind and revision. */
export function EditRuleDialog({
    rule,
    destinations,
    destinationMore,
    onClose,
    onSaved,
}: {
    /** The row as last read; it updates when the list refetches. */
    rule: Schemas["NotificationRule"];
    destinations: readonly Schemas["NotificationDestination"][];
    destinationMore?: ReactNode;
    onClose: () => void;
    /** Announces the saved rule once the dialog is gone. */
    onSaved: (name: string) => void;
}) {
    const client = useQueryClient();
    // Freeze the row this edit opened. A refreshed prop must not move the
    // revision under an unchanged draft; only an explicit reload adopts it.
    const [baseline, setBaseline] = useState(rule);
    const [draft, setDraft] = useState(() => ruleDraftOf(rule));
    const [conflicted, setConflicted] = useState(false);
    const body = updateRuleBody(baseline, draft);
    const update = useSessionMutation({
        mutationFn: (request: Schemas["UpdateNotificationRule"]) =>
            call(
                api.PATCH("/api/v1/notifications/rules/{id}", {
                    params: {
                        path: { id: baseline.id },
                        header: csrfHeader(),
                    },
                    body: request,
                }),
            ),
        onSuccess: (updated) => {
            void client.invalidateQueries({
                queryKey: ["notifications", "rules"],
            });
            onSaved(updated.name);
            onClose();
        },
        onError: (error) => {
            if (isRevisionConflict(error)) {
                setConflicted(true);
                // The row must show the revision the next attempt names.
                void client.invalidateQueries({
                    queryKey: ["notifications", "rules"],
                });
            }
        },
    });
    const blocked = update.isPending || uncertain(update.error);
    // Invalidation refreshes the list; only a changed revision can be adopted.
    function reload() {
        if (rule.revision === baseline.revision) {
            void client.invalidateQueries({
                queryKey: ["notifications", "rules"],
            });
            return;
        }
        setBaseline(rule);
        setDraft(ruleDraftOf(rule));
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
                        Revision {baseline.revision}. The event kind is fixed; a
                        saved change applies to future events.
                    </DialogDescription>
                </DialogHeader>
                {conflicted && (
                    <Alert role="alert" variant="warning">
                        <TriangleAlertIcon aria-hidden="true" />
                        <AlertTitle>
                            This rule changed while you were editing.
                        </AlertTitle>
                        <AlertDescription>
                            <p>
                                Nothing was saved. Your edits are still here;
                                revision {rule.revision} is current.
                            </p>
                            <Button
                                variant="outline"
                                size="sm"
                                onClick={reload}
                            >
                                Reload revision {rule.revision}
                            </Button>
                        </AlertDescription>
                    </Alert>
                )}
                <form
                    onSubmit={(event) => {
                        event.preventDefault();
                        if (body !== null && !blocked && !conflicted) {
                            update.mutate(body);
                        }
                    }}
                >
                    <fieldset
                        disabled={blocked}
                        className="m-0 grid gap-4 border-0 p-0"
                    >
                        <RuleFields
                            value={draft}
                            onChange={(next) => {
                                setDraft(next);
                                update.reset();
                            }}
                            destinations={destinations}
                            destinationMore={destinationMore}
                            lockedEventKind
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
                                    body === null || blocked || conflicted
                                }
                            >
                                {update.isPending ? "Saving…" : "Save rule"}
                            </Button>
                        </DialogFooter>
                    </fieldset>
                </form>
                <WriteError error={update.error} />
            </DialogContent>
        </Dialog>
    );
}
