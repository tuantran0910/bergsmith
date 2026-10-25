// The policy fields shared by the create and edit dialogs. The documented
// ranges live in `docs/operations/api.md`; they are repeated here as input
// bounds so the browser refuses an impossible value before the round trip,
// while the coordinator stays the authority on what it accepts. Each hint is
// a description of its control, not part of its label.

import { useId, type ReactNode } from "react";

import { Input, Label } from "../../ui/Input";
import type { PolicyDraft } from "./draft";

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

/** The fields one policy owns; the enabled flag appears only when editing. */
export function PolicyFields({
    value,
    onChange,
    showEnabled,
    disabled,
}: {
    value: PolicyDraft;
    onChange: (value: PolicyDraft) => void;
    /** Edit offers the enabled flag; a created policy always starts enabled. */
    showEnabled: boolean;
    disabled: boolean;
}) {
    const prefix = useId();
    const name = `${prefix}-name`;
    const target = `${prefix}-target`;
    const minimum = `${prefix}-minimum`;
    const budget = `${prefix}-budget`;
    return (
        <div className="grid gap-4 sm:grid-cols-2">
            <div className="sm:col-span-2">
                <Field
                    id={name}
                    label="Policy name"
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
            </div>
            <div className="grid content-start gap-1.5 text-sm">
                <span className="font-medium">Operation</span>
                <p className="m-0 text-muted-foreground">Binpack compaction</p>
                <span className="text-xs text-muted-foreground">
                    This release implements one operation.
                </span>
            </div>
            {showEnabled && (
                <label className="flex items-center gap-2 self-end pb-1 text-sm">
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
                    Enabled for scheduled runs
                </label>
            )}
            <Field
                id={target}
                label="Target file size (bytes)"
                hint="Between 1 MiB and 1 GiB. Default 256 MiB."
            >
                <Input
                    id={target}
                    aria-describedby={`${target}-hint`}
                    required
                    type="number"
                    inputMode="numeric"
                    min={1_048_576}
                    max={1_073_741_824}
                    step={1}
                    value={value.target}
                    onChange={(event) => {
                        onChange({ ...value, target: event.target.value });
                    }}
                    disabled={disabled}
                />
            </Field>
            <Field
                id={minimum}
                label="Minimum input files"
                hint="Between 2 and 1000. Default 5."
            >
                <Input
                    id={minimum}
                    aria-describedby={`${minimum}-hint`}
                    required
                    type="number"
                    inputMode="numeric"
                    min={2}
                    max={1000}
                    step={1}
                    value={value.minimum}
                    onChange={(event) => {
                        onChange({ ...value, minimum: event.target.value });
                    }}
                    disabled={disabled}
                />
            </Field>
            <div className="sm:col-span-2">
                <Field
                    id={budget}
                    label="Run budget (bytes)"
                    hint="The most input one run may rewrite: between 1 MiB and 100 GiB, and at least the target file size. Default 100 GiB."
                >
                    <Input
                        id={budget}
                        aria-describedby={`${budget}-hint`}
                        required
                        type="number"
                        inputMode="numeric"
                        min={1_048_576}
                        max={107_374_182_400}
                        step={1}
                        value={value.budget}
                        onChange={(event) => {
                            onChange({ ...value, budget: event.target.value });
                        }}
                        disabled={disabled}
                    />
                </Field>
            </div>
        </div>
    );
}
