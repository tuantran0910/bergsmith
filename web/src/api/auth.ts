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

/** A permission name the coordinator checks. */
export type Permission =
    | "inventory:read"
    | "jobs:read"
    | "artifacts:read"
    | "jobs:submit"
    | "jobs:cancel"
    | "jobs:retry"
    | "connections:manage"
    | "schedules:manage"
    | "workers:manage"
    | "notifications:manage"
    | "grants:manage"
    | "audit:read";

/**
 * One permission the session may exercise, over the scope its grant covers.
 * The coordinator expands roles into these pairs; the browser must not read
 * them through role names, because a role may gain or lose a permission.
 */
export interface SessionPermission {
    permission: Permission;
    scope: GrantScope;
}

/** `GET /auth/session`. */
export interface SessionInfo {
    organization_id: string;
    principal_id: string;
    display_name: string;
    expires_at: string;
    grants: SessionGrant[];
    permissions: SessionPermission[];
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

/** Whether the session holds `permission` over the whole organization. */
export function hasOrganizationPermission(
    session: SessionInfo,
    permission: Permission,
): boolean {
    return session.permissions.some(
        (held) =>
            held.permission === permission &&
            held.scope.catalog_connection === null &&
            held.scope.table === null,
    );
}

/**
 * Whether the session holds `permission` over `connection`: an
 * organization-wide grant covers every connection, and a connection grant
 * covers only itself. A table grant never covers the connection.
 */
export function hasConnectionPermission(
    session: SessionInfo,
    permission: Permission,
    connection: string,
): boolean {
    return session.permissions.some(
        (held) =>
            held.permission === permission &&
            held.scope.table === null &&
            (held.scope.catalog_connection === null ||
                held.scope.catalog_connection === connection),
    );
}

/**
 * Whether the session holds `permission` over `table`, reached through
 * `connections`: an organization grant covers every table, a table grant only
 * its own table, and a connection grant every table named on that connection.
 */
export function hasTablePermission(
    session: SessionInfo,
    permission: Permission,
    table: string,
    connections: Iterable<string>,
): boolean {
    const reached = new Set(connections);
    return session.permissions.some(
        (held) =>
            held.permission === permission &&
            (held.scope.table === table ||
                (held.scope.table === null &&
                    (held.scope.catalog_connection === null ||
                        reached.has(held.scope.catalog_connection)))),
    );
}

/**
 * Whether the session can read organization audit history. Only an
 * organization-wide grant carries `audit:read`.
 */
export function canReadAudit(session: SessionInfo): boolean {
    return hasOrganizationPermission(session, "audit:read");
}

/**
 * Whether the session can list worker sessions. Only an organization-wide
 * grant carries `workers:manage`.
 */
export function canManageWorkers(session: SessionInfo): boolean {
    return hasOrganizationPermission(session, "workers:manage");
}

/**
 * Whether the session can administer organization maintenance configuration
 * (policies and schedules). Only an organization-wide grant carries
 * `schedules:manage`; a grant narrowed to a table or connection never
 * administers organization configuration.
 */
export function canManageSchedules(session: SessionInfo): boolean {
    return hasOrganizationPermission(session, "schedules:manage");
}

/**
 * Whether the session can manage one connection's settings, or create a
 * connection when `id` is omitted. Creation needs an organization-wide grant;
 * editing a connection accepts a grant over that connection.
 */
export function canManageCatalog(session: SessionInfo, id?: string): boolean {
    if (id === undefined) {
        return hasOrganizationPermission(session, "connections:manage");
    }
    return hasConnectionPermission(session, "connections:manage", id);
}
