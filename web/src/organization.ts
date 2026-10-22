// The organization this browser last signed in to. `/auth/session` does not
// name the organization's slug, so the sign-in page remembers it to prefill
// the form and to offer sign-in again before the session expires. It is a
// per-browser convenience: storage that is missing or refuses access means
// the slug is simply not known.

/** The `localStorage` key that keeps the slug. */
export const ORGANIZATION_STORAGE_KEY = "bergsmith.organization";

/** The remembered slug, or `null` when none is stored or readable. */
export function readLastOrganization(): string | null {
    try {
        const stored = window.localStorage.getItem(ORGANIZATION_STORAGE_KEY);
        return stored === null || stored.trim() === "" ? null : stored;
    } catch {
        return null;
    }
}

/** Remembers `slug`; a browser that refuses storage forgets it. */
export function saveLastOrganization(slug: string): void {
    try {
        window.localStorage.setItem(ORGANIZATION_STORAGE_KEY, slug);
    } catch {
        // Storage is optional; sign-in works without it.
    }
}
