import { type LucideIcon, MonitorIcon, MoonIcon, SunIcon } from "lucide-react";

import {
    THEME_PREFERENCES,
    type ThemePreference,
    useThemePreference,
} from "../theme";
import { SegmentedControl } from "../ui/SegmentedControl";

const OPTIONS: Record<ThemePreference, { label: string; icon: LucideIcon }> = {
    system: { label: "System theme", icon: MonitorIcon },
    light: { label: "Light theme", icon: SunIcon },
    dark: { label: "Dark theme", icon: MoonIcon },
};

type ThemeToggleProps =
    | {
          preference: ThemePreference;
          onChange: (preference: ThemePreference) => void;
      }
    | { preference?: undefined; onChange?: undefined };

/** Chooses the system, light, or dark theme; the current choice is pressed. */
export function ThemeToggle(props: ThemeToggleProps = {}) {
    const [stored, chooseStored] = useThemePreference();
    const preference = props.preference ?? stored;
    const choose = props.onChange ?? chooseStored;
    return (
        <SegmentedControl
            label="Theme"
            iconOnly
            value={preference}
            onChange={choose}
            options={THEME_PREFERENCES.map((option) => ({
                value: option,
                ...OPTIONS[option],
            }))}
        />
    );
}
