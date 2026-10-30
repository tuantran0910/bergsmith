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

/**
 * A labelled status whose tone shows as text, an icon, and a color. `topic`
 * names what the status is about ("Execution: Running") where the badge
 * stands apart from its explanation, such as in a page header.
 */
export function StatusBadge({
    status,
    topic,
}: {
    status: StatusLine;
    topic?: string;
}) {
    const { variant, icon: Icon } = TONES[status.tone];
    return (
        <Badge variant={variant} data-tone={status.tone}>
            <Icon aria-hidden="true" />
            {topic === undefined ? status.label : `${topic}: ${status.label}`}
        </Badge>
    );
}
