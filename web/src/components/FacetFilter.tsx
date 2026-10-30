// A list's facet filters (ADR 0067): a dimension's values with the counts the
// coordinator returns, chosen with native checkboxes or radios, and the
// active choices as removable pills.
//
// Inline, a facet opens from a button in a modal popover; inside the filter
// bar's narrow drawer it renders its choices in place, since the drawer is
// already a dialog. Counts follow the session's grants (ADR 0067), so they
// never reveal a resource the list would hide.

import { ChevronDownIcon, XIcon } from "lucide-react";
import { useId, useState } from "react";

import { number } from "../format";
import { Button } from "../ui/Button";
import { Popover, PopoverContent, PopoverTrigger } from "../ui/Popover";
import { useMediaQuery } from "./useMediaQuery";

export interface FacetOption {
    value: string;
    label: string;
    /** Rows with this value under the other filters; absent while loading. */
    count?: number | undefined;
}

/**
 * The options to offer: every value with a count, in the coordinator's
 * order, then any selected value it no longer counts, with zero, so a
 * selection never disappears from its own filter.
 */
export function facetOptions(
    counts: readonly { value: string; count: number }[] | undefined,
    selected: readonly string[],
    label: (value: string) => string,
): FacetOption[] {
    const options: FacetOption[] = (counts ?? []).map((entry) => ({
        value: entry.value,
        label: label(entry.value),
        count: entry.count,
    }));
    const listed = new Set(options.map((option) => option.value));
    for (const value of selected) {
        if (!listed.has(value)) {
            options.push({
                value,
                label: label(value),
                count: counts === undefined ? undefined : 0,
            });
        }
    }
    return options;
}

function Choices({
    name,
    options,
    selected,
    multiple,
    allLabel,
    onChange,
}: {
    name: string;
    options: readonly FacetOption[];
    selected: readonly string[];
    multiple: boolean;
    allLabel: string;
    onChange: (values: string[]) => void;
}) {
    const choice =
        "flex min-h-7 cursor-pointer items-center gap-2 rounded-sm px-1.5 hover:bg-muted";
    const input = "size-3.5 accent-foreground";
    return (
        <div className="grid gap-0.5">
            {!multiple && (
                <label className={choice}>
                    <input
                        type="radio"
                        name={name}
                        className={input}
                        checked={selected.length === 0}
                        onChange={() => {
                            onChange([]);
                        }}
                    />
                    <span className="flex-1">{allLabel}</span>
                </label>
            )}
            {options.map((option) => {
                const checked = selected.includes(option.value);
                return (
                    <label key={option.value} className={choice}>
                        <input
                            type={multiple ? "checkbox" : "radio"}
                            name={name}
                            className={input}
                            checked={checked}
                            onChange={() => {
                                if (!multiple) {
                                    onChange([option.value]);
                                } else if (checked) {
                                    onChange(
                                        selected.filter(
                                            (value) => value !== option.value,
                                        ),
                                    );
                                } else {
                                    onChange([...selected, option.value]);
                                }
                            }}
                        />
                        <span className="min-w-0 flex-1 break-words">
                            {option.label}
                        </span>{" "}
                        {option.count !== undefined && (
                            <span className="text-xs text-muted-foreground tabular-nums">
                                {number(option.count)}
                            </span>
                        )}
                    </label>
                );
            })}
            {options.length === 0 && (
                <p className="m-0 px-1.5 py-1 text-muted-foreground">
                    No values to filter by.
                </p>
            )}
        </div>
    );
}

/**
 * One dimension's filter. `multiple` offers checkboxes (the values combine
 * with OR); otherwise radios, with an "all" choice that clears it.
 */
export function FacetFilter({
    label,
    options,
    selected,
    onChange,
    multiple = false,
    allLabel = "All",
}: {
    label: string;
    options: readonly FacetOption[];
    selected: readonly string[];
    onChange: (values: string[]) => void;
    multiple?: boolean;
    allLabel?: string;
}) {
    const name = useId();
    const narrow = !useMediaQuery("(min-width: 40rem)");
    // The page applies a choice through the address, which settles after
    // the click; until `selected` changes, show the choice as made.
    const [pending, setPending] = useState<readonly string[] | null>(null);
    const [seen, setSeen] = useState(selected);
    if (selected !== seen && selected.join(",") !== seen.join(",")) {
        setSeen(selected);
        setPending(null);
    }
    const current = pending ?? selected;
    const choices = (
        <Choices
            name={name}
            options={options}
            selected={current}
            multiple={multiple}
            allLabel={allLabel}
            onChange={(values) => {
                setPending(values);
                onChange(values);
            }}
        />
    );

    if (narrow) {
        return (
            <fieldset className="m-0 grid gap-1 border-0 p-0">
                <legend className="mb-1 p-0 text-sm font-semibold">
                    {label}
                </legend>
                {choices}
            </fieldset>
        );
    }

    const summary =
        current.length === 0
            ? undefined
            : current.length === 1
              ? options.find((option) => option.value === current[0])?.label
              : `${number(current.length)} selected`;
    return (
        <Popover modal>
            <PopoverTrigger render={<Button variant="outline" size="sm" />}>
                {/* "filter" follows the visible text, so the accessible
                    name starts with what the button shows (WCAG 2.5.3). */}
                {label}
                {summary !== undefined && (
                    <>
                        {" "}
                        <span className="font-normal text-muted-foreground">
                            {summary}
                        </span>
                    </>
                )}{" "}
                <span className="sr-only">filter</span>
                <ChevronDownIcon aria-hidden="true" />
            </PopoverTrigger>
            <PopoverContent className="w-60 p-1">
                <fieldset className="m-0 border-0 p-0">
                    <legend className="px-1.5 pt-1 pb-1.5 text-xs font-semibold text-muted-foreground">
                        {label}
                    </legend>
                    {choices}
                </fieldset>
            </PopoverContent>
        </Popover>
    );
}

export interface FilterPill {
    key: string;
    /** The dimension, for example `State`. */
    label: string;
    /** The chosen value's label. */
    value: string;
    onRemove: () => void;
}

/** The active filters as pills; each removes its own filter. */
export function FilterPills({ pills }: { pills: readonly FilterPill[] }) {
    if (pills.length === 0) {
        return null;
    }
    return (
        <ul
            aria-label="Active filters"
            className="m-0 flex list-none flex-wrap gap-1.5 p-0"
        >
            {pills.map((pill) => (
                <li key={pill.key}>
                    <span className="inline-flex h-6 items-center gap-1 rounded-sm bg-muted pr-0.5 pl-2 text-xs">
                        <span className="text-muted-foreground">
                            {pill.label}
                        </span>
                        <span>{pill.value}</span>
                        <button
                            type="button"
                            aria-label={`Remove filter ${pill.label}: ${pill.value}`}
                            className="inline-flex size-5 items-center justify-center rounded-sm outline-none hover:bg-background focus-visible:ring-3 focus-visible:ring-ring/50"
                            onClick={pill.onRemove}
                        >
                            <XIcon aria-hidden="true" className="size-3" />
                        </button>
                    </span>
                </li>
            ))}
        </ul>
    );
}
