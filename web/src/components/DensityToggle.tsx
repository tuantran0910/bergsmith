import { Rows3Icon, Rows4Icon, type LucideIcon } from "lucide-react";

import { DENSITIES, type Density, useDensityPreference } from "../density";
import { Button } from "../ui/Button";

const OPTIONS: Record<Density, { label: string; icon: LucideIcon }> = {
    comfortable: { label: "Comfortable density", icon: Rows3Icon },
    compact: { label: "Compact density", icon: Rows4Icon },
};

/** Chooses comfortable or compact table rows; the current choice is pressed. */
export function DensityToggle({
    density: controlled,
    onChange,
}: {
    density?: Density;
    onChange?: (density: Density) => void;
} = {}) {
    const [stored, chooseStored] = useDensityPreference();
    const density = controlled ?? stored;
    const choose = onChange ?? chooseStored;
    return (
        <div
            role="group"
            aria-label="Density"
            className="inline-flex rounded-lg border border-border p-0.5"
        >
            {DENSITIES.map((option) => {
                const { label, icon: Icon } = OPTIONS[option];
                return (
                    <Button
                        key={option}
                        variant="ghost"
                        size="icon-sm"
                        aria-label={label}
                        aria-pressed={density === option}
                        title={label}
                        onClick={() => {
                            choose(option);
                        }}
                    >
                        <Icon aria-hidden="true" />
                    </Button>
                );
            })}
        </div>
    );
}
