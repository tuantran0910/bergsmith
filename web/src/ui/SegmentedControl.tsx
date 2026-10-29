import type { LucideIcon } from "lucide-react";

import { cn } from "./cn";

/** One option of a segmented control. */
export interface Segment<Value extends string> {
    value: Value;
    /** The accessible name; also the visible text unless `iconOnly`. */
    label: string;
    icon?: LucideIcon;
}

/**
 * A row of mutually exclusive options, exactly one pressed. Each option is a
 * native toggle button with `aria-pressed`, so it needs no script beyond the
 * click and stays in the first-load bundle at almost no cost; the shell
 * renders it on every page.
 */
export function SegmentedControl<Value extends string>({
    label,
    value,
    options,
    onChange,
    iconOnly = false,
    className,
}: {
    /** Names the group for assistive technology. */
    label: string;
    value: Value;
    options: readonly Segment<Value>[];
    onChange: (value: Value) => void;
    iconOnly?: boolean;
    className?: string;
}) {
    return (
        <div
            role="group"
            aria-label={label}
            className={cn(
                "inline-flex w-fit items-center gap-0.5 rounded-md border bg-muted p-0.5",
                className,
            )}
        >
            {options.map(
                ({ value: option, label: optionLabel, icon: Icon }) => (
                    <button
                        key={option}
                        type="button"
                        aria-pressed={option === value}
                        aria-label={iconOnly ? optionLabel : undefined}
                        title={iconOnly ? optionLabel : undefined}
                        onClick={() => {
                            if (option !== value) {
                                onChange(option);
                            }
                        }}
                        className={cn(
                            "inline-flex h-6 items-center justify-center gap-1.5 rounded-sm px-2 text-xs text-muted-foreground transition-colors duration-(--duration-instant) outline-none hover:text-foreground focus-visible:ring-3 focus-visible:ring-ring/50 aria-pressed:bg-background aria-pressed:text-foreground aria-pressed:shadow-xs [&_svg]:size-3.5 [&_svg]:shrink-0",
                            iconOnly && "w-6 px-0",
                        )}
                    >
                        {Icon !== undefined && <Icon aria-hidden="true" />}
                        {!iconOnly && optionLabel}
                    </button>
                ),
            )}
        </div>
    );
}
