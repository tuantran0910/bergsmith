// The lazily loaded half of `AccountButton`: a modal popover, open while
// mounted, anchored to the button that requested it.

import { Popover as PopoverPrimitive } from "@base-ui/react/popover";
import { LogOutIcon } from "lucide-react";
import type { RefObject } from "react";

import type { Density } from "../density";
import type { ThemePreference } from "../theme";
import { Button } from "../ui/Button";
import { PopoverContent, PopoverTitle } from "../ui/Popover";
import { Separator } from "../ui/Separator";
import { DensityToggle } from "./DensityToggle";
import { ThemeToggle } from "./ThemeToggle";

export interface AccountPanelProps {
    id: string;
    anchor: RefObject<HTMLButtonElement | null>;
    side: "top" | "right";
    displayName: string;
    theme: ThemePreference;
    onThemeChange: (preference: ThemePreference) => void;
    density: Density;
    onDensityChange: (density: Density) => void;
    logout: { isPending: boolean; mutate: () => void };
    /** `pressedButton` is true when an outside press landed on the anchor. */
    onOpenChange: (open: boolean, pressedButton: boolean) => void;
}

export default function AccountPanel({
    id,
    anchor,
    side,
    displayName,
    theme,
    onThemeChange,
    density,
    onDensityChange,
    logout,
    onOpenChange,
}: AccountPanelProps) {
    return (
        <PopoverPrimitive.Root
            defaultOpen
            modal
            onOpenChange={(open, details) => {
                const target = details.event.target;
                const pressedButton =
                    target instanceof Node &&
                    anchor.current?.contains(target) === true;
                onOpenChange(open, pressedButton);
            }}
        >
            <PopoverContent
                id={id}
                anchor={anchor}
                side={side}
                align={side === "top" ? "start" : "end"}
                finalFocus={anchor}
                className="flex w-60 flex-col gap-3 p-2"
            >
                <PopoverTitle className="truncate px-1 pt-1">
                    {displayName}
                </PopoverTitle>
                <div className="flex items-center justify-between gap-2 px-1">
                    <span className="text-xs text-muted-foreground">Theme</span>
                    <ThemeToggle preference={theme} onChange={onThemeChange} />
                </div>
                <div className="flex items-center justify-between gap-2 px-1">
                    <span className="text-xs text-muted-foreground">
                        Density
                    </span>
                    <DensityToggle
                        density={density}
                        onChange={onDensityChange}
                    />
                </div>
                <Separator />
                <Button
                    variant="ghost"
                    size="sm"
                    className="justify-start"
                    disabled={logout.isPending}
                    onClick={() => {
                        logout.mutate();
                    }}
                >
                    <LogOutIcon aria-hidden="true" />
                    Sign out
                </Button>
            </PopoverContent>
        </PopoverPrimitive.Root>
    );
}
