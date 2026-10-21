import type { StatusLine } from "../jobs/status";

/** A labelled status with a tone that does not rely on color alone. */
export function StatusBadge({ status }: { status: StatusLine }) {
    return <span className={`badge badge-${status.tone}`}>{status.label}</span>;
}
