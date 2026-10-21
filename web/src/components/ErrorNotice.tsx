import { ApiFailure } from "../api/client";
import { words } from "../format";

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
    return (
        <div role="alert" className="notice notice-error">
            <strong>{reason}</strong>
            {action && <p>Next step: {action}.</p>}
            {failure?.retryable && onRetry && (
                <button type="button" onClick={onRetry}>
                    Retry the same request
                </button>
            )}
        </div>
    );
}
