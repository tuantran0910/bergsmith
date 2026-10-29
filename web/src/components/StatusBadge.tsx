import {
    CircleAlertIcon,
    CircleCheckIcon,
    CircleDashedIcon,
    CircleXIcon,
    LoaderCircleIcon,
    type LucideIcon,
} from "lucide-react";

import type { StatusLine, Tone } from "../jobs/status";
import { Badge, type BadgeProps } from "../ui/Badge";

const TONES: Record<
    Tone,
    { variant: NonNullable<BadgeProps["variant"]>; icon: LucideIcon }
> = {
    ok: { variant: "success", icon: CircleCheckIcon },
    warning: { variant: "warning", icon: CircleAlertIcon },
    error: { variant: "destructive", icon: CircleXIcon },
    progress: { variant: "running", icon: LoaderCircleIcon },
    neutral: { variant: "neutral", icon: CircleDashedIcon },
};

/** A labelled status whose tone shows as text, an icon, and a color. */
export function StatusBadge({ status }: { status: StatusLine }) {
    const { variant, icon: Icon } = TONES[status.tone];
    return (
        <Badge variant={variant} data-tone={status.tone}>
            <Icon aria-hidden="true" />
            {status.label}
        </Badge>
    );
}
