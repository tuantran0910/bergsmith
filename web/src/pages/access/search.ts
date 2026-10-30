// The principal pages' URL state (ADR 0056): the detail's open tab and the
// user list's filters. Both stay in validated search parameters so a link
// opens the same view, while an unknown tab falls back to access.

import { flagParam, searchText } from "../../components/listSearch";

const TABS = ["access", "tokens"] as const;

/** The principal detail's addressable tab. */
export type PrincipalTab = (typeof TABS)[number];

export interface PrincipalSearch {
    tab?: PrincipalTab;
}

/** The tab in a search parameter, or `undefined` for anything else. */
export function principalTab(value: unknown): PrincipalTab | undefined {
    return TABS.find((candidate) => candidate === value);
}

/** Keeps unknown or malformed principal-detail search parameters out of the page. */
export function validatePrincipalSearch(
    search: Record<string, unknown>,
): PrincipalSearch {
    const selected = principalTab(search["tab"]);
    return selected === undefined ? {} : { tab: selected };
}

/** The user list's filters (ADR 0067). */
export interface UsersSearch {
    q?: string;
    active?: "true" | "false";
}

/** Keeps unknown or malformed user-list search parameters out of the page. */
export function validateUsersSearch(
    search: Record<string, unknown>,
): UsersSearch {
    const q = searchText(search["q"]);
    const active = flagParam(search["active"]);
    return {
        ...(q !== undefined ? { q } : {}),
        ...(active !== undefined ? { active } : {}),
    };
}
