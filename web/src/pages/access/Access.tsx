import { useState } from "react";
import type { AccessScope } from "../../api/access";
import { useSession } from "../../queryClient";
import { Button } from "../../ui/Button";
import { canManageScope, Grants } from "./Grants";

/** Load administrative data only when an authorized operator opens Access. */
export function Access({
    scope,
    connections = [],
}: {
    scope: AccessScope;
    connections?: string[];
}) {
    const session = useSession();
    const [open, setOpen] = useState(false);
    if (!session || !canManageScope(session, scope, connections)) return null;
    return (
        <section className="mt-6 grid gap-4" aria-label="Access">
            <div>
                <Button
                    variant="outline"
                    aria-expanded={open}
                    onClick={() => {
                        setOpen(!open);
                    }}
                >
                    {open ? "Hide access" : "Access"}
                </Button>
            </div>
            {open && <Grants scope={scope} connections={connections} />}
        </section>
    );
}
