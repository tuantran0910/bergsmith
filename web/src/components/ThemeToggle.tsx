import { type LucideIcon, MonitorIcon, MoonIcon, SunIcon } from "lucide-react";

import {
    THEME_PREFERENCES,
    type ThemePreference,
    useThemePreference,
} from "../theme";
import { Button } from "../ui/Button";

const OPTIONS: Record<ThemePreference, { label: string; icon: LucideIcon }> = {
    system: { label: "System theme", icon: MonitorIcon },
    light: { label: "Light theme", icon: SunIcon },
    dark: { label: "Dark theme", icon: MoonIcon },
};

/** Chooses the system, light, or dark theme; the current choice is pressed. */
export function ThemeToggle() {
    const [preference, choose] = useThemePreference();
    return (
        <div
            role="group"
            aria-label="Theme"
            className="inline-flex rounded-lg border border-border p-0.5"
        >
            {THEME_PREFERENCES.map((option) => {
                const { label, icon: Icon } = OPTIONS[option];
                return (
                    <Button
                        key={option}
                        variant="ghost"
                        size="icon-sm"
                        aria-label={label}
                        aria-pressed={preference === option}
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
