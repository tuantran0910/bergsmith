import type { AccessScope } from "../../api/access";
import { useSession } from "../../queryClient";
import { canManageScope, Grants } from "./Grants";

/**
 * The Access tab's body. Administrative data loads only when an authorized
 * operator opens the tab; a caller without the scope grant sees nothing.
 */
export function Access({
    scope,
    connections = [],
    scopeLabel,
}: {
    scope: AccessScope;
    connections?: string[];
    scopeLabel: string;
}) {
    const session = useSession();
    if (!session || !canManageScope(session, scope, connections)) return null;
    return (
        <section aria-label="Access">
            <Grants
                scope={scope}
                connections={connections}
                scopeLabel={scopeLabel}
            />
        </section>
    );
}
