// A breadcrumb trail derived from the current address (ADR 0056). The
// leader is always the overview; each section and resource follows.
// Resources display their short identifier until the names-first pass
// reaches them; the full form is the hover text.

import { Link, useRouterState } from "@tanstack/react-router";

import { shortId } from "../format";

const SECTION_LABELS: Record<string, string> = {
    catalogs: "Catalogs",
    jobs: "Jobs",
    tables: "Tables",
    "service-identities": "Service identities",
    users: "Users",
    workers: "Workers",
};

/** Pages that create a resource describe their purpose instead of an ID. */
const CREATE_LABELS: Record<string, string> = {
    "/catalogs/new": "Connect a catalog",
};

interface Crumb {
    /** Undefined for the current crumb. */
    to?: string;
    label: string;
    /** The full identifier on hover for truncated values. */
    title?: string;
}

/** Crumbs for an address; unknown paths get none. */
export function crumbsFor(pathname: string): Crumb[] {
    const [, first, second] = pathname.split("/");
    if (first === undefined) {
        return [];
    }
    const sectionLabel = SECTION_LABELS[first];
    if (sectionLabel === undefined) {
        return first === "" ? [{ label: "Overview" }] : [];
    }
    const overview: Crumb = { to: "/", label: "Overview" };
    const section: Crumb = { label: sectionLabel };
    if (second === undefined || second === "") {
        return [overview, { ...section }];
    }
    const createLabel = CREATE_LABELS[`/${first}/${second}`];
    if (createLabel !== undefined) {
        return [
            overview,
            { to: `/${first}`, label: section.label },
            { label: createLabel },
        ];
    }
    return [
        overview,
        { to: `/${first}`, label: section.label },
        { label: shortId(second), title: second },
    ];
}

/** The page's location trail; hidden on the overview itself. */
export function Breadcrumbs() {
    const pathname = useRouterState({ select: (s) => s.location.pathname });
    const crumbs = crumbsFor(pathname);
    if (crumbs.length <= 1) {
        return null;
    }
    return (
        <nav aria-label="Breadcrumb">
            <ol className="m-0 flex list-none flex-wrap items-center gap-1 p-0 text-sm text-muted-foreground">
                {crumbs.map((crumb, index) => {
                    const last = index === crumbs.length - 1;
                    return (
                        <li
                            key={`${crumb.label}-${index}`}
                            className="flex items-center gap-1"
                        >
                            {index > 0 && (
                                <span aria-hidden="true" className="opacity-60">
                                    /
                                </span>
                            )}
                            {crumb.to === undefined || last ? (
                                <span
                                    aria-current={last ? "page" : undefined}
                                    title={crumb.title}
                                >
                                    {crumb.label}
                                </span>
                            ) : (
                                <Link
                                    to={crumb.to}
                                    className="hover:text-foreground hover:no-underline"
                                >
                                    {crumb.label}
                                </Link>
                            )}
                        </li>
                    );
                })}
            </ol>
        </nav>
    );
}
