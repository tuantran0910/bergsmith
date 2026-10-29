import { Rows3Icon, Rows4Icon, type LucideIcon } from "lucide-react";

import { DENSITIES, type Density, useDensityPreference } from "../density";
import { SegmentedControl } from "../ui/SegmentedControl";

const OPTIONS: Record<Density, { label: string; icon: LucideIcon }> = {
    comfortable: { label: "Comfortable density", icon: Rows3Icon },
    compact: { label: "Compact density", icon: Rows4Icon },
};

type DensityToggleProps =
    | {
          density: Density;
          onChange: (density: Density) => void;
      }
    | { density?: undefined; onChange?: undefined };

/** Chooses comfortable or compact table rows; the current choice is pressed. */
export function DensityToggle(props: DensityToggleProps = {}) {
    const [stored, chooseStored] = useDensityPreference();
    const density = props.density ?? stored;
    const choose = props.onChange ?? chooseStored;
    return (
        <SegmentedControl
            label="Density"
            iconOnly
            value={density}
            onChange={choose}
            options={DENSITIES.map((option) => ({
                value: option,
                ...OPTIONS[option],
            }))}
        />
    );
}
