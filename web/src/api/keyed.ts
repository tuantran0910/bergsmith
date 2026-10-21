import { useRef } from "react";

import { useSessionMutation } from "../queryClient";
import { ApiFailure, idempotencyKey } from "./client";

/**
 * A mutation that carries an `Idempotency-Key` for one user action.
 *
 * The key is created on the first attempt and reused while the outcome is
 * uncertain: the API said the same request may be retried, or no typed answer
 * arrived (a network or gateway failure, which can follow a committed job).
 * Retrying then replays the original operation instead of creating a second
 * job. Success or a typed permanent failure ends the action; the next attempt
 * gets a new key.
 */
export function useKeyedMutation<TData, TVariables>(
    perform: (key: string, variables: TVariables) => Promise<TData>,
    onSuccess: (data: TData) => void,
) {
    const key = useRef<string | null>(null);
    return useSessionMutation<TData, TVariables>({
        mutationFn: (variables) => {
            key.current ??= idempotencyKey();
            return perform(key.current, variables);
        },
        onSuccess: (data) => {
            key.current = null;
            onSuccess(data);
        },
        onError: (error) => {
            if (!(error instanceof ApiFailure) || !error.outcomeUncertain) {
                key.current = null;
            }
        },
    });
}
