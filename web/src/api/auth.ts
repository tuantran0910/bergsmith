// The `/auth` routes. They belong to the identity router, not the `/api/v1`
// contract, so these two calls are typed here by hand.

import { ApiFailure, csrfHeader } from "./client";

export interface GrantScope {
    catalog_connection: string | null;
    table: string | null;
}

export interface SessionGrant {
    role: string;
    scope: GrantScope;
}

/** `GET /auth/session`. */
export interface SessionInfo {
    organization_id: string;
    principal_id: string;
    display_name: string;
    expires_at: string;
    grants: SessionGrant[];
}

/** The current session, or `null` when the browser has none. */
export async function fetchSession(
    signal?: AbortSignal,
): Promise<SessionInfo | null> {
    let response: Response;
    try {
        response = await fetch("/auth/session", {
            credentials: "same-origin",
            ...(signal === undefined ? {} : { signal }),
        });
    } catch {
        throw new ApiFailure(0, null);
    }
    if (response.status === 401) {
        return null;
    }
    if (!response.ok) {
        throw new ApiFailure(response.status, null);
    }
    return (await response.json()) as SessionInfo;
}

/** Revokes the session. An already-invalid session counts as signed out. */
export async function signOut(): Promise<void> {
    let response: Response;
    try {
        response = await fetch("/auth/logout", {
            method: "POST",
            credentials: "same-origin",
            headers: csrfHeader(),
        });
    } catch {
        throw new ApiFailure(0, null);
    }
    if (!response.ok && response.status !== 401) {
        throw new ApiFailure(response.status, null);
    }
}

/** Where the browser goes to sign in to `organization`, then back to `returnTo`. */
export function loginUrl(organization: string, returnTo: string): string {
    const query = new URLSearchParams({ organization, return_to: returnTo });
    return `/auth/login?${query.toString()}`;
}

/**
 * Whether the session can read organization audit history: only an
 * organization-wide Administrator grant carries `audit:read`.
 */
export function canReadAudit(session: SessionInfo): boolean {
    return session.grants.some(
        (grant) =>
            grant.role === "Administrator" &&
            grant.scope.catalog_connection === null &&
            grant.scope.table === null,
    );
}
