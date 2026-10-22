import { LockIcon } from "lucide-react";
import type { ReactNode } from "react";

import { Alert, AlertDescription, AlertTitle } from "../ui/Alert";

/** Explains which grant a page needs when the coordinator refused it. */
export function GrantRequired({ children }: { children: ReactNode }) {
    return (
        <Alert role="alert" variant="warning">
            <LockIcon aria-hidden="true" />
            <AlertTitle>Permission required</AlertTitle>
            <AlertDescription>
                <p>{children}</p>
            </AlertDescription>
        </Alert>
    );
}
