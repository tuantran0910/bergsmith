// Facts inside a peek panel (ADR 0067): labels and values as a description
// list, so the panel reads as one record.

import type { ReactNode } from "react";

/** Label and value pairs; a `null` value is left out. */
export function PeekFacts({
    facts,
}: {
    facts: readonly (readonly [string, ReactNode | null])[];
}) {
    return (
        <dl className="m-0 grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-1">
            {facts
                .filter(([, value]) => value !== null)
                .map(([label, value]) => (
                    <div key={label} className="contents">
                        <dt className="text-muted-foreground">{label}</dt>
                        <dd className="m-0 min-w-0 break-words">{value}</dd>
                    </div>
                ))}
        </dl>
    );
}
