import { CircleXIcon, RotateCcwIcon } from "lucide-react";

import { ApiFailure } from "../api/client";
import { words } from "../format";
import { Alert, AlertDescription, AlertTitle } from "../ui/Alert";
import { Button } from "../ui/Button";

interface Props {
    error: unknown;
    /** Sends the same request again; offered only when that is safe. */
    onRetry?: () => void;
}

/** Shows an API failure's typed reason and the operator action it names. */
export function ErrorNotice({ error, onRetry }: Props) {
    const failure = error instanceof ApiFailure ? error : null;
    const reason = failure?.body
        ? words(failure.body.reason)
        : failure?.status === 0
          ? "The coordinator could not be reached"
          : `Request failed${failure ? ` (${failure.status})` : ""}`;
    const action = failure?.body ? words(failure.body.operator_action) : null;
    const retry = failure?.retryable === true && onRetry !== undefined;
    return (
        <Alert role="alert" variant="destructive" className="my-3">
            <CircleXIcon aria-hidden="true" />
            <AlertTitle>{reason}</AlertTitle>
            {(action !== null || retry) && (
                <AlertDescription>
                    {action !== null && <p>Next step: {action}.</p>}
                    {retry && (
                        <Button variant="outline" size="sm" onClick={onRetry}>
                            <RotateCcwIcon aria-hidden="true" />
                            Retry the same request
                        </Button>
                    )}
                </AlertDescription>
            )}
        </Alert>
    );
}
