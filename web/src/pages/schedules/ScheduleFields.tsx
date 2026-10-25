// The schedule fields. Timing is shared by create and edit; the bound policy,
// table, and service identity are chosen once at creation and are read-only
// afterwards, because the coordinator refuses to change them.

import { useId } from "react";

import { Input, Label } from "../../ui/Input";
import { Select } from "../../ui/Select";
import type { ScheduleDraft } from "./draft";

/** Zones offered as suggestions; any regional IANA name is accepted. */
const COMMON_ZONES = [
    "UTC",
    "Europe/Berlin",
    "Europe/London",
    "Europe/Warsaw",
    "America/New_York",
    "America/Chicago",
    "America/Los_Angeles",
    "Asia/Singapore",
    "Asia/Tokyo",
    "Australia/Sydney",
];

interface Option {
    value: string;
    label: string;
}

/** The choices a creator must make; the edit form shows the bound values. */
export function SchedulePickers({
    draft,
    onChange,
    disabled,
    policies,
    tables,
    services,
    moreTables,
    loadingTables,
    onLoadTables,
}: {
    draft: ScheduleDraft;
    onChange: (draft: ScheduleDraft) => void;
    disabled: boolean;
    policies: readonly Option[];
    tables: readonly Option[];
    services: readonly Option[];
    moreTables: boolean;
    loadingTables: boolean;
    onLoadTables: () => void;
}) {
    const prefix = useId();
    return (
        <div className="grid gap-4 sm:grid-cols-2">
            <div className="grid content-start gap-1.5">
                <Label htmlFor={`${prefix}-policy`}>Policy</Label>
                <Select
                    id={`${prefix}-policy`}
                    value={draft.policyId}
                    onValueChange={(policyId) => {
                        onChange({ ...draft, policyId });
                    }}
                    options={[
                        { value: "", label: "Choose a policy" },
                        ...policies,
                    ]}
                />
                <span className="text-xs text-muted-foreground">
                    The run budget and target file size belong to the policy.
                </span>
            </div>
            <div className="grid content-start gap-1.5">
                <Label htmlFor={`${prefix}-service`}>Service identity</Label>
                <Select
                    id={`${prefix}-service`}
                    value={draft.serviceId}
                    onValueChange={(serviceId) => {
                        onChange({ ...draft, serviceId });
                    }}
                    options={[
                        { value: "", label: "Choose a service identity" },
                        ...services,
                    ]}
                />
            </div>
            <div className="grid content-start gap-1.5 sm:col-span-2">
                <Label htmlFor={`${prefix}-table`}>Table</Label>
                <Select
                    id={`${prefix}-table`}
                    value={draft.tableId}
                    onValueChange={(tableId) => {
                        onChange({ ...draft, tableId });
                    }}
                    options={[
                        { value: "", label: "Choose a table" },
                        ...tables,
                    ]}
                />
                {moreTables && (
                    <button
                        type="button"
                        className="w-fit text-xs font-medium text-primary underline-offset-4 hover:underline disabled:opacity-50"
                        disabled={disabled || loadingTables}
                        onClick={onLoadTables}
                    >
                        {loadingTables
                            ? "Loading more tables…"
                            : "Load more tables"}
                    </button>
                )}
            </div>
        </div>
    );
}

/** Cron, zone, the optional daily window, and the enabled flag. */
export function ScheduleTimingFields({
    draft,
    onChange,
    disabled,
    showEnabled,
}: {
    draft: ScheduleDraft;
    onChange: (draft: ScheduleDraft) => void;
    disabled: boolean;
    showEnabled: boolean;
}) {
    const prefix = useId();
    const cron = `${prefix}-cron`;
    const zone = `${prefix}-zone`;
    return (
        <div className="grid gap-4 sm:grid-cols-2">
            <div className="grid content-start gap-1.5 sm:col-span-2">
                <Label htmlFor={cron}>Cron expression</Label>
                <Input
                    id={cron}
                    required
                    maxLength={200}
                    value={draft.cron}
                    onChange={(event) => {
                        onChange({ ...draft, cron: event.target.value });
                    }}
                    autoComplete="off"
                    spellCheck={false}
                    disabled={disabled}
                />
                <span className="text-xs text-muted-foreground">
                    Five fields: minute, hour, day of month, month, day of week,
                    in the schedule's time zone. For example{" "}
                    <code>0 2 * * MON-FRI</code>.
                </span>
            </div>
            <div className="grid content-start gap-1.5">
                <Label htmlFor={zone}>Time zone</Label>
                <Input
                    id={zone}
                    required
                    maxLength={64}
                    list={`${zone}-suggestions`}
                    value={draft.zone}
                    onChange={(event) => {
                        onChange({ ...draft, zone: event.target.value });
                    }}
                    autoComplete="off"
                    spellCheck={false}
                    disabled={disabled}
                />
                <datalist id={`${zone}-suggestions`}>
                    {COMMON_ZONES.map((name) => (
                        <option key={name} value={name} />
                    ))}
                </datalist>
                <span className="text-xs text-muted-foreground">
                    UTC or a regional IANA name with a slash. Abbreviations such
                    as EST are refused because they ignore daylight saving.
                </span>
            </div>
            {showEnabled && (
                <label className="flex items-center gap-2 self-end pb-1 text-sm">
                    <input
                        type="checkbox"
                        checked={draft.enabled}
                        onChange={(event) => {
                            onChange({
                                ...draft,
                                enabled: event.target.checked,
                            });
                        }}
                        disabled={disabled}
                    />
                    Enabled for scheduled runs
                </label>
            )}
            <fieldset className="m-0 grid gap-3 border-0 p-0 sm:col-span-2">
                <legend className="p-0 text-sm font-medium">
                    Daily window
                </legend>
                <label className="flex items-center gap-2 text-sm">
                    <input
                        type="checkbox"
                        checked={draft.windowed}
                        onChange={(event) => {
                            onChange({
                                ...draft,
                                windowed: event.target.checked,
                            });
                        }}
                        disabled={disabled}
                    />
                    Only start runs inside a daily window
                </label>
                {draft.windowed && (
                    <div className="grid gap-3 sm:grid-cols-2">
                        <Label>
                            Window opens
                            <Input
                                type="time"
                                required
                                value={draft.start}
                                onChange={(event) => {
                                    onChange({
                                        ...draft,
                                        start: event.target.value,
                                    });
                                }}
                                disabled={disabled}
                            />
                        </Label>
                        <Label>
                            Window closes
                            <Input
                                type="time"
                                required
                                value={draft.end}
                                onChange={(event) => {
                                    onChange({
                                        ...draft,
                                        end: event.target.value,
                                    });
                                }}
                                disabled={disabled}
                            />
                        </Label>
                        <p className="m-0 text-xs text-muted-foreground sm:col-span-2">
                            Local time in the schedule's zone. The opening is
                            inclusive and the closing exclusive; a later closing
                            time wraps past midnight, so a window may open at
                            22:00 and close at 02:00. A start equal to the close
                            names no window.
                        </p>
                    </div>
                )}
            </fieldset>
        </div>
    );
}
