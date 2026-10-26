// A table's evaluated health as a labeled, toned badge. The text carries the
// state, so colour is never the only signal.

import { Badge } from "../ui/Badge";

/** The evaluated state a page may show; `null` is "not assessed". */
export type HealthState = "healthy" | "warning" | "critical" | null;

export function HealthBadge({ state }: { state: HealthState }) {
    switch (state) {
        case "critical":
            return <Badge variant="destructive">Critical</Badge>;
        case "warning":
            return <Badge variant="warning">Warning</Badge>;
        case "healthy":
            return <Badge variant="success">Healthy</Badge>;
        default:
            return <Badge variant="neutral">Not assessed</Badge>;
    }
}
