import {
    MutationCache,
    QueryCache,
    QueryClient,
    useMutation,
    useQuery,
    useQueryClient,
} from "@tanstack/react-query";

import { fetchSession, type SessionInfo } from "./api/auth";
import { ApiFailure } from "./api/client";

/** The query holding the current session, or `null` when signed out. */
export const SESSION_KEY = ["session"] as const;

/** Who the cached data belongs to: organization and principal. */
export function identityOf(session: SessionInfo | null | undefined): string {
    return session ? `${session.organization_id}/${session.principal_id}` : "";
}

/**
 * The session generation of each client. It advances whenever the identity
 * that cached data belongs to ends: sign-out, a 401, or a different
 * principal. Requests remember the generation they started in.
 */
const generations = new WeakMap<QueryClient, number>();

/** The client's current session generation. */
export function sessionGeneration(client: QueryClient): number {
    return generations.get(client) ?? 0;
}

/**
 * A request finished after the session it started in ended. Its result
 * belongs to nobody on screen, so no callback or global handler acts on it.
 */
export class ObsoleteSession extends Error {
    constructor() {
        super("the session that started this request has ended");
        this.name = "ObsoleteSession";
    }
}

/**
 * Runs `request` for the current session. If the session generation changes
 * before it settles, its result or failure is replaced by
 * {@link ObsoleteSession}.
 */
export async function forSession<T>(
    client: QueryClient,
    request: () => Promise<T>,
): Promise<T> {
    const generation = sessionGeneration(client);
    let result: T;
    try {
        result = await request();
    } catch (error) {
        if (sessionGeneration(client) !== generation) {
            throw new ObsoleteSession();
        }
        throw error;
    }
    if (sessionGeneration(client) !== generation) {
        throw new ObsoleteSession();
    }
    return result;
}

/**
 * Ends the current session generation: every cached read and mutation
 * result is dropped, and requests still in flight become obsolete. Only the
 * session query survives.
 */
export function clearProtected(client: QueryClient): void {
    generations.set(client, sessionGeneration(client) + 1);
    const filters = {
        predicate: (query: { queryKey: readonly unknown[] }) =>
            query.queryKey[0] !== SESSION_KEY[0],
    };
    void client.cancelQueries(filters);
    client.removeQueries(filters);
    client.getMutationCache().clear();
}

/**
 * Reads `/auth/session` for the current generation. If sign-out or a
 * different tab's sign-in ends that generation while the request is in
 * flight, the answer belongs to nobody on screen: it must not run the
 * identity check or {@link clearProtected} against whatever replaced it, so
 * it settles as {@link ObsoleteSession} instead.
 */
export async function readSession(
    client: QueryClient,
    signal?: AbortSignal,
): Promise<SessionInfo | null> {
    const generation = sessionGeneration(client);
    const next = await fetchSession(signal);
    if (sessionGeneration(client) !== generation) {
        throw new ObsoleteSession();
    }
    // Another tab may have signed in as someone else: nothing read for the
    // previous identity may stay visible.
    const previous = client.getQueryData<SessionInfo | null>(SESSION_KEY);
    if (previous !== undefined && identityOf(previous) !== identityOf(next)) {
        clearProtected(client);
    }
    return next;
}

/**
 * Records `next` as the session, clearing caches if the identity changed.
 * Signing out also aborts session reads still in flight, so a response from
 * before cannot sign the browser back in.
 */
export function setSession(
    client: QueryClient,
    next: SessionInfo | null,
): void {
    const previous = client.getQueryData<SessionInfo | null>(SESSION_KEY);
    if (next === null) {
        void client.cancelQueries({ queryKey: SESSION_KEY });
    }
    if (identityOf(previous) !== identityOf(next)) {
        clearProtected(client);
    }
    client.setQueryData(SESSION_KEY, next);
}

/**
 * The current session, subscribed so a refreshed permission set rerenders the
 * caller. Pages gate controls on permissions, and `getQueryData` only reads a
 * snapshot: a memoized router outlet can keep a page that used it from seeing
 * the root query's newer session.
 */
export function useSession(): SessionInfo | null {
    const client = useQueryClient();
    return (
        useQuery({
            queryKey: SESSION_KEY,
            queryFn: ({ signal }) => readSession(client, signal),
            // The root query owns the refresh, but this observer must ask for
            // it too so its own mount or focus always finds the same answer.
            refetchOnWindowFocus: true,
        }).data ?? null
    );
}

/**
 * A mutation bound to the session it starts in. If the session ends first,
 * it settles as {@link ObsoleteSession}: no success callback runs, and its
 * failure never reaches the global 401 handling.
 */
export function useSessionMutation<TData, TVariables = void>(options: {
    mutationFn: (variables: TVariables) => Promise<TData>;
    onSuccess?: (data: TData) => void;
    onError?: (error: unknown) => void;
}) {
    const client = useQueryClient();
    return useMutation<TData, unknown, TVariables>({
        ...options,
        mutationFn: (variables) =>
            forSession(client, () => options.mutationFn(variables)),
    });
}

/**
 * Builds the query client. A 401 anywhere means the session ended, so the
 * application forgets what it read and returns to sign-in. Only failures the
 * API marks as safe to repeat are retried, and mutations are never retried
 * automatically. `retryDelay` overrides the backoff, for tests.
 */
export function createQueryClient(retryDelay?: number): QueryClient {
    const client: QueryClient = new QueryClient({
        queryCache: new QueryCache({ onError: signedOut }),
        mutationCache: new MutationCache({ onError: signedOut }),
        defaultOptions: {
            queries: {
                retry: (failures, error) =>
                    failures < 2 &&
                    error instanceof ApiFailure &&
                    error.retryable,
                refetchOnWindowFocus: false,
                ...(retryDelay === undefined ? {} : { retryDelay }),
            },
            mutations: { retry: false },
        },
    });

    // An obsolete request's 401 is an `ObsoleteSession`, not an `ApiFailure`,
    // so it cannot sign out the session that replaced it.
    function signedOut(error: unknown): void {
        if (error instanceof ApiFailure && error.status === 401) {
            setSession(client, null);
        }
    }

    return client;
}
